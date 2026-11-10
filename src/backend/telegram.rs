//! Сетевой воркер: клиент grammers на однопоточном рантайме tokio.
//!
//! Команды исполняются отдельными локальными задачами, чтобы долгий запрос
//! не задерживал ни обновления, ни следующие команды.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::{DialogIter, LoginToken, PasswordToken, UpdateStream};
use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_client::peer::{Peer, User};
use grammers_client::sender::{ConnectionParams, UpdatesConfiguration};
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SenderPool, SignInError, tl};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::{JoinSet, LocalSet};
use tokio::time::{sleep, timeout};

use super::{
    AuthState, ChatInfo, ChatKind, Command, DialogData, Emit, Event, MessageData, Presence,
};
use crate::config::{self, Credentials};
use crate::format::Clock;
use crate::session::FileSession;

const DIALOG_PAGE: usize = 100;
const HISTORY_PAGE: usize = 50;
const RETRY_DELAY: Duration = Duration::from_secs(3);
const SESSION_FILE: &str = "session";

type Commands = UnboundedReceiver<Command>;

/// Чем закончилась работа с аккаунтом.
enum Outcome {
    Quit,
    LoggedOut,
    BadCredentials,
}

/// Почему фоновая задача просит остановить работу с аккаунтом.
#[derive(Clone, Copy)]
enum Stop {
    /// Сервер больше не признаёт сессию: её завершили с другого устройства.
    Revoked,
    /// Пул соединений остановился, запросы больше не выполняются.
    Dead,
}

struct Shared {
    client: Client,
    emit: Emit,
    me: i64,
    peers: RefCell<HashMap<i64, PeerRef>>,
    /// Итератор списка чатов; `None`, пока грузится страница или список кончился.
    dialogs: RefCell<Option<DialogIter>>,
    online: Cell<bool>,
    stop: Notify,
    stop_reason: Cell<Option<Stop>>,
}

type Ctx = Rc<Shared>;

pub fn run(commands: Commands, emit: Emit, dir: PathBuf) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            emit(Event::Fatal(format!("Не удалось запустить сетевой модуль: {error}")));
            return;
        }
    };
    LocalSet::new().block_on(&runtime, main_loop(commands, emit, dir));
}

async fn main_loop(mut commands: Commands, emit: Emit, dir: PathBuf) {
    if let Err(error) = config::ensure_dir(&dir) {
        emit(Event::Fatal(format!("Нет доступа к каталогу данных {}: {error}", dir.display())));
        return;
    }
    let mut use_env = true;
    let mut rejected = false;
    loop {
        let stored = Credentials::load(&dir).or_else(|| use_env.then(Credentials::from_env).flatten());
        let credentials = match stored {
            Some(credentials) => credentials,
            None => {
                emit(Event::Auth(AuthState::NeedCredentials));
                if rejected {
                    emit(Event::AuthError(
                        "Telegram не принял эти api_id и api_hash. Проверьте их на my.telegram.org".into(),
                    ));
                }
                match ask_credentials(&mut commands, &emit, &dir).await {
                    Some(credentials) => credentials,
                    None => return,
                }
            }
        };
        match run_account(&mut commands, &emit, &dir, &credentials).await {
            Outcome::Quit => return,
            Outcome::LoggedOut => {}
            Outcome::BadCredentials => {
                Credentials::forget(&dir);
                use_env = false;
                rejected = true;
            }
        }
    }
}

async fn ask_credentials(commands: &mut Commands, emit: &Emit, dir: &Path) -> Option<Credentials> {
    loop {
        match commands.recv().await? {
            Command::Quit => return None,
            Command::SetCredentials { api_id, api_hash } => {
                let Some(credentials) = Credentials::parse(&api_id, &api_hash) else {
                    emit(Event::AuthError("api_id — это число, api_hash — 32 символа из цифр и букв a–f".into()));
                    continue;
                };
                if let Err(error) = credentials.save(dir) {
                    emit(Event::Error(format!("Не удалось сохранить учётные данные: {error}")));
                }
                return Some(credentials);
            }
            _ => {}
        }
    }
}

async fn run_account(commands: &mut Commands, emit: &Emit, dir: &Path, credentials: &Credentials) -> Outcome {
    emit(Event::Auth(AuthState::Connecting));
    let session_path = dir.join(SESSION_FILE);
    let session = Arc::new(FileSession::open(&session_path));
    let params = ConnectionParams { app_version: env!("CARGO_PKG_VERSION").to_owned(), ..Default::default() };
    let SenderPool { runner, handle, updates } =
        SenderPool::with_configuration(session, credentials.api_id, params);
    let client = Client::new(handle);
    let pool = tokio::spawn(runner.run());

    let outcome = async {
        let me = match authorize(commands, emit, &client, &credentials.api_hash).await {
            Ok(me) => me,
            Err(outcome) => return outcome,
        };
        let stream = match client.stream_updates(updates, UpdatesConfiguration { catch_up: false }).await {
            Ok(stream) => stream,
            Err(error) => {
                emit(Event::Fatal(format!("Не удалось подписаться на обновления: {error}")));
                return Outcome::Quit;
            }
        };
        serve(commands, emit, &client, me, stream).await
    }
    .await;

    client.disconnect();
    let _ = timeout(Duration::from_secs(2), pool).await;
    if !matches!(outcome, Outcome::Quit) {
        FileSession::remove(&session_path);
    }
    outcome
}

/// Ждёт указанное время; истина, если за это время попросили закрыть приложение.
async fn quit_within(commands: &mut Commands, delay: Duration) -> bool {
    let pause = sleep(delay);
    tokio::pin!(pause);
    loop {
        tokio::select! {
            _ = &mut pause => return false,
            command = commands.recv() => {
                if matches!(command, None | Some(Command::Quit)) {
                    return true;
                }
            }
        }
    }
}

/// Возвращает свой аккаунт: из сохранённой сессии или после входа.
async fn authorize(commands: &mut Commands, emit: &Emit, client: &Client, api_hash: &str) -> Result<User, Outcome> {
    loop {
        let attempt = match client.is_authorized().await {
            Ok(true) => client.get_me().await.map(Some),
            Ok(false) => Ok(None),
            Err(error) => Err(error),
        };
        match attempt {
            Ok(Some(me)) => return Ok(me),
            Ok(None) => return login(commands, emit, client, api_hash).await,
            Err(error) if is_bad_api_id(&error) => return Err(Outcome::BadCredentials),
            Err(error) if is_unauthorized(&error) => return login(commands, emit, client, api_hash).await,
            Err(error) => {
                emit(Event::AuthError(format!("{}. Повторяю попытку…", describe(&error))));
                if quit_within(commands, RETRY_DELAY).await {
                    return Err(Outcome::Quit);
                }
                emit(Event::Auth(AuthState::Connecting));
            }
        }
    }
}

async fn login(commands: &mut Commands, emit: &Emit, client: &Client, api_hash: &str) -> Result<User, Outcome> {
    emit(Event::Auth(AuthState::NeedPhone));
    let mut code_token: Option<LoginToken> = None;
    let mut password_token: Option<PasswordToken> = None;
    loop {
        let Some(command) = commands.recv().await else { return Err(Outcome::Quit) };
        match command {
            Command::Quit => return Err(Outcome::Quit),
            Command::AuthBack => {
                code_token = None;
                password_token = None;
                emit(Event::Auth(AuthState::NeedPhone));
            }
            Command::SubmitPhone(phone) => {
                let phone: String = phone.chars().filter(|ch| ch.is_ascii_digit() || *ch == '+').collect();
                match client.request_login_code(&phone, api_hash).await {
                    Ok(token) => {
                        code_token = Some(token);
                        emit(Event::Auth(AuthState::NeedCode { phone }));
                    }
                    Err(error) if is_bad_api_id(&error) => return Err(Outcome::BadCredentials),
                    Err(error) => emit(Event::AuthError(describe(&error))),
                }
            }
            Command::SubmitCode(code) => {
                let Some(token) = &code_token else { continue };
                match client.sign_in(token, code.trim()).await {
                    Ok(me) => return Ok(me),
                    Err(SignInError::PasswordRequired(token)) => {
                        let hint = token.hint().unwrap_or_default().to_owned();
                        password_token = Some(token);
                        emit(Event::Auth(AuthState::NeedPassword { hint }));
                    }
                    Err(SignInError::InvalidCode) => {
                        emit(Event::AuthError("Неверный или просроченный код".into()));
                    }
                    Err(SignInError::SignUpRequired) => emit(Event::AuthError(
                        "Этот номер не зарегистрирован. Создайте аккаунт в официальном приложении Telegram".into(),
                    )),
                    Err(SignInError::Other(error)) => emit(Event::AuthError(describe(&error))),
                    Err(SignInError::InvalidPassword(_)) => {}
                }
            }
            Command::SubmitPassword(password) => {
                let Some(token) = password_token.take() else { continue };
                match client.check_password(token, password.as_bytes()).await {
                    Ok(me) => return Ok(me),
                    Err(SignInError::InvalidPassword(token)) => {
                        password_token = Some(token);
                        emit(Event::AuthError("Неверный пароль".into()));
                    }
                    Err(error) => {
                        // Данные для проверки пароля одноразовые: начинаем вход заново.
                        code_token = None;
                        emit(Event::Auth(AuthState::NeedPhone));
                        emit(Event::AuthError(match error {
                            SignInError::Other(error) => describe(&error),
                            other => other.to_string(),
                        }));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Основной режим: раздаёт команды задачам и следит за потоком обновлений.
async fn serve(commands: &mut Commands, emit: &Emit, client: &Client, me: User, stream: UpdateStream) -> Outcome {
    let ctx: Ctx = Rc::new(Shared {
        client: client.clone(),
        emit: emit.clone(),
        me: me.id().bot_api_dialog_id().unwrap_or_default(),
        peers: RefCell::new(HashMap::new()),
        dialogs: RefCell::new(Some(client.iter_dialogs())),
        online: Cell::new(true),
        stop: Notify::new(),
        stop_reason: Cell::new(None),
    });
    ctx.emit(Event::Ready { me: ctx.me, name: display_name(&me) });

    let mut tasks = JoinSet::new();
    let updates_task = tasks.spawn_local(updates_loop(ctx.clone(), stream)).id();
    tasks.spawn_local(load_dialogs(ctx.clone()));

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return Outcome::Quit };
                match command {
                    Command::Quit => return Outcome::Quit,
                    Command::Logout => {
                        // Локальную сессию удаляем, даже если сервер не ответил.
                        let _ = timeout(Duration::from_secs(5), ctx.client.sign_out()).await;
                        ctx.emit(Event::LoggedOut);
                        return Outcome::LoggedOut;
                    }
                    Command::LoadDialogs => drop(tasks.spawn_local(load_dialogs(ctx.clone()))),
                    Command::LoadHistory { chat, before } => {
                        tasks.spawn_local(load_history(ctx.clone(), chat, before));
                    }
                    Command::RefreshPreview(chat) => drop(tasks.spawn_local(refresh_preview(ctx.clone(), chat))),
                    Command::Send { chat, text, local_id } => {
                        tasks.spawn_local(send(ctx.clone(), chat, text, local_id));
                    }
                    Command::MarkRead(chat) => drop(tasks.spawn_local(mark_read(ctx.clone(), chat))),
                    Command::Typing(chat) => drop(tasks.spawn_local(send_typing(ctx.clone(), chat))),
                    _ => {}
                }
            }
            _ = ctx.stop.notified() => match ctx.stop_reason.get() {
                Some(Stop::Dead) => {
                    ctx.emit(Event::Fatal("Соединение с Telegram остановилось. Перезапустите приложение".into()));
                    return Outcome::Quit;
                }
                _ => {
                    ctx.emit(Event::LoggedOut);
                    ctx.emit(Event::Error("Сессия завершена. Войдите снова".into()));
                    return Outcome::LoggedOut;
                }
            },
            Some(finished) = tasks.join_next_with_id() => {
                let (id, panicked) = match finished {
                    Ok((id, ())) => (id, false),
                    Err(error) => (error.id(), error.is_panic()),
                };
                if id == updates_task {
                    ctx.emit(Event::Fatal("Приём обновлений остановился. Перезапустите приложение".into()));
                    return Outcome::Quit;
                }
                if panicked {
                    ctx.emit(Event::Error("Внутренняя ошибка сетевого модуля".into()));
                }
            }
        }
    }
}

async fn updates_loop(ctx: Ctx, mut stream: UpdateStream) {
    loop {
        match stream.next().await {
            Ok(update) => {
                ctx.set_online(true);
                ctx.handle_update(update).await;
            }
            Err(error) => {
                if ctx.check_stop(&error) {
                    return std::future::pending().await;
                }
                ctx.set_online(false);
                // Поток сам запросит пропущенное, как только связь вернётся.
                let mut delay = Duration::from_secs(2);
                loop {
                    sleep(delay).await;
                    match ctx.client.is_authorized().await {
                        Ok(true) => break ctx.set_online(true),
                        Ok(false) => {
                            ctx.request_stop(Stop::Revoked);
                            return std::future::pending().await;
                        }
                        Err(_) => delay = (delay * 2).min(Duration::from_secs(30)),
                    }
                }
            }
        }
    }
}

async fn load_dialogs(ctx: Ctx) {
    let Some(mut iter) = ctx.dialogs.borrow_mut().take() else { return };
    let now = Clock::now();
    let mut items = Vec::with_capacity(DIALOG_PAGE);
    let mut done = false;
    let mut fetched = 0;
    while fetched < DIALOG_PAGE {
        match iter.next().await {
            Ok(Some(dialog)) => {
                fetched += 1;
                ctx.set_online(true);
                let tl::enums::Dialog::Dialog(raw) = &dialog.raw else { continue };
                let peer = dialog.peer();
                let (Some(id), Ok(Some(reference))) = (peer.id().bot_api_dialog_id(), peer.to_ref().await) else {
                    continue;
                };
                ctx.peers.borrow_mut().insert(id, reference);
                let tl::enums::PeerNotifySettings::Settings(notify) = &raw.notify_settings;
                items.push(DialogData {
                    id,
                    info: chat_info(peer, now),
                    unread: raw.unread_count.max(i32::from(raw.unread_mark)),
                    pinned: raw.pinned,
                    muted: is_muted(notify, now),
                    archived: raw.folder_id == Some(1),
                    read_outbox_max: raw.read_outbox_max_id,
                    last: dialog.last_message.as_ref().and_then(|message| ctx.message_data(message)),
                });
            }
            Ok(None) => {
                done = true;
                break;
            }
            Err(error) => {
                if ctx.check_stop(&error) {
                    return;
                }
                // Без списка чатов клиент бесполезен, поэтому пробуем до победы.
                ctx.set_online(false);
                sleep(RETRY_DELAY).await;
            }
        }
    }
    if !done {
        *ctx.dialogs.borrow_mut() = Some(iter);
    }
    ctx.emit(Event::Dialogs { items, done });
}

async fn load_history(ctx: Ctx, chat: i64, before: Option<i32>) {
    let Some(peer) = ctx.peer(chat) else {
        return ctx.emit(Event::HistoryFailed { chat });
    };
    for attempt in 1..=3 {
        let mut iter = ctx.client.iter_messages(peer).limit(HISTORY_PAGE);
        if let Some(id) = before {
            iter = iter.offset_id(id);
        }
        let mut messages = Vec::with_capacity(HISTORY_PAGE);
        let mut fetched = 0;
        let result = loop {
            match iter.next().await {
                Ok(Some(message)) => {
                    fetched += 1;
                    messages.extend(ctx.message_data(&message));
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        match result {
            Ok(()) => {
                // Сервер отдаёт от новых к старым, интерфейсу нужен обратный порядок.
                messages.reverse();
                let end = fetched < HISTORY_PAGE;
                return ctx.emit(Event::History { chat, messages, older: before.is_some(), end });
            }
            Err(error) if attempt == 3 || matches!(error, InvocationError::Rpc(_)) => {
                ctx.report(&error);
                return ctx.emit(Event::HistoryFailed { chat });
            }
            Err(_) => sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn refresh_preview(ctx: Ctx, chat: i64) {
    let Some(peer) = ctx.peer(chat) else { return };
    let mut iter = ctx.client.iter_messages(peer).limit(1);
    match iter.next().await {
        Ok(message) => {
            let last = message.and_then(|message| ctx.message_data(&message));
            ctx.emit(Event::Preview { chat, last });
        }
        Err(error) => {
            ctx.check_stop(&error);
        }
    }
}

async fn send(ctx: Ctx, chat: i64, text: String, local_id: i32) {
    let result = match ctx.peer(chat) {
        Some(peer) => ctx.client.send_message(peer, text).await,
        None => Err(InvocationError::Dropped("unknown chat")),
    };
    match result {
        Ok(message) => {
            // Если ответ сервера не удалось разобрать, сообщение придёт обновлением.
            let message = ctx.message_data(&message).filter(|message| message.id > 0);
            ctx.emit(Event::Sent { chat, local_id, message });
        }
        Err(error) => {
            ctx.report(&error);
            ctx.emit(Event::SendFailed { chat, local_id });
        }
    }
}

async fn mark_read(ctx: Ctx, chat: i64) {
    let Some(peer) = ctx.peer(chat) else { return };
    if let Err(error) = ctx.client.mark_as_read(peer).await {
        ctx.check_stop(&error);
    }
}

async fn send_typing(ctx: Ctx, chat: i64) {
    let Some(peer) = ctx.peer(chat) else { return };
    let action = tl::enums::SendMessageAction::SendMessageTypingAction;
    let _ = ctx.client.action(peer).oneshot(action).await;
}

impl Shared {
    fn emit(&self, event: Event) {
        (self.emit)(event);
    }

    fn peer(&self, chat: i64) -> Option<PeerRef> {
        self.peers.borrow().get(&chat).copied()
    }

    fn set_online(&self, online: bool) {
        if self.online.replace(online) != online {
            self.emit(Event::Connection(online));
        }
    }

    fn request_stop(&self, reason: Stop) {
        self.stop_reason.set(Some(reason));
        self.stop.notify_one();
    }

    /// Истина, если ошибка означает, что работать с аккаунтом дальше нельзя.
    fn check_stop(&self, error: &InvocationError) -> bool {
        let reason = if is_unauthorized(error) {
            Stop::Revoked
        } else if matches!(error, InvocationError::Dropped(reason) if *reason != "unknown chat") {
            Stop::Dead
        } else {
            return false;
        };
        self.request_stop(reason);
        true
    }

    /// Показывает ошибку пользователю, если это не остановка работы.
    fn report(&self, error: &InvocationError) {
        if !self.check_stop(error) {
            self.emit(Event::Error(describe(error)));
        }
    }

    fn message_data(&self, message: &Message) -> Option<MessageData> {
        let (text, media, service) = match &message.raw {
            tl::enums::Message::Empty(_) => return None,
            tl::enums::Message::Service(raw) => (service_text(message, &raw.action, self.me), String::new(), true),
            tl::enums::Message::Message(raw) => {
                let media = match message.media() {
                    Some(media) => media_label(&media),
                    None if raw.media.as_ref().is_some_and(|media| *media != tl::enums::MessageMedia::Empty) => {
                        "Вложение".to_owned()
                    }
                    None => String::new(),
                };
                (raw.message.clone(), media, false)
            }
        };
        let chat = message.peer_id().bot_api_dialog_id()?;
        // У своих сообщений в личных чатах отправитель обозначен особым идентификатором.
        let sender_id = match message.sender_id() {
            Some(id) => id.bot_api_dialog_id().unwrap_or(self.me),
            None => chat,
        };
        Some(MessageData {
            id: message.id(),
            chat,
            date: message.date().as_second(),
            outgoing: message.outgoing(),
            sender_id,
            sender: message.sender().map(peer_name).unwrap_or_default(),
            text,
            media,
            service,
            edited: message.edit_date().is_some() && !message.edit_hide(),
        })
    }

    async fn handle_update(&self, update: Update) {
        match update {
            Update::NewMessage(message) => {
                let Some(data) = self.message_data(&message) else { return };
                if self.peer(data.chat).is_none()
                    && let Ok(Some(reference)) = message.peer_ref().await
                {
                    self.peers.borrow_mut().insert(data.chat, reference);
                }
                let info = match message.peer() {
                    Some(peer) => chat_info(peer, Clock::now()),
                    None => ChatInfo {
                        kind: if data.chat > 0 { ChatKind::User } else { ChatKind::Group },
                        title: "Чат".to_owned(),
                        presence: Presence::Unknown,
                        members: None,
                        can_write: true,
                    },
                };
                self.emit(Event::NewMessage { message: data, info });
            }
            Update::MessageEdited(message) => {
                if let Some(data) = self.message_data(&message) {
                    self.emit(Event::MessageEdited(data));
                }
            }
            Update::MessageDeleted(deletion) => {
                let chat = deletion.channel_id().and_then(channel_dialog_id);
                self.emit(Event::MessagesDeleted { chat, ids: deletion.into_messages() });
            }
            Update::Raw(raw) => self.handle_raw(&raw.raw),
            _ => {}
        }
    }

    fn handle_raw(&self, update: &tl::enums::Update) {
        use tl::enums::Update as U;
        let event = match update {
            U::ReadHistoryOutbox(u) => dialog_id(&u.peer).map(|chat| Event::ReadOutbox { chat, max_id: u.max_id }),
            U::ReadChannelOutbox(u) => {
                channel_dialog_id(u.channel_id).map(|chat| Event::ReadOutbox { chat, max_id: u.max_id })
            }
            U::ReadHistoryInbox(u) => {
                dialog_id(&u.peer).map(|chat| Event::ReadInbox { chat, unread: u.still_unread_count })
            }
            U::ReadChannelInbox(u) => {
                channel_dialog_id(u.channel_id).map(|chat| Event::ReadInbox { chat, unread: u.still_unread_count })
            }
            U::UserTyping(u) => self.typing(Some(u.user_id), Some(u.user_id), &u.action),
            U::ChatUserTyping(u) => {
                let chat = PeerId::chat(u.chat_id).and_then(PeerId::bot_api_dialog_id);
                self.typing(chat, dialog_id(&u.from_id), &u.action)
            }
            U::ChannelUserTyping(u) => {
                self.typing(channel_dialog_id(u.channel_id), dialog_id(&u.from_id), &u.action)
            }
            U::UserStatus(u) => {
                Some(Event::Presence { user: u.user_id, presence: presence(&u.status, Clock::now()) })
            }
            U::DialogPinned(u) => match &u.peer {
                tl::enums::DialogPeer::Peer(peer) => {
                    dialog_id(&peer.peer).map(|chat| Event::Pinned { chat, pinned: u.pinned })
                }
                _ => None,
            },
            U::NotifySettings(u) => match &u.peer {
                tl::enums::NotifyPeer::Peer(peer) => {
                    let tl::enums::PeerNotifySettings::Settings(notify) = &u.notify_settings;
                    let muted = is_muted(notify, Clock::now());
                    dialog_id(&peer.peer).map(|chat| Event::Muted { chat, muted })
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(event) = event {
            self.emit(event);
        }
    }

    fn typing(&self, chat: Option<i64>, user: Option<i64>, action: &tl::enums::SendMessageAction) -> Option<Event> {
        let (chat, user) = (chat?, user?);
        let cancelled = matches!(action, tl::enums::SendMessageAction::SendMessageCancelAction);
        (!cancelled && user != self.me).then_some(Event::Typing { chat, user })
    }
}

fn dialog_id(peer: &tl::enums::Peer) -> Option<i64> {
    PeerId::from(peer).bot_api_dialog_id()
}

fn channel_dialog_id(channel: i64) -> Option<i64> {
    PeerId::channel(channel)?.bot_api_dialog_id()
}

fn is_muted(settings: &tl::types::PeerNotifySettings, now: i64) -> bool {
    settings.mute_until.is_some_and(|until| i64::from(until) > now)
}

fn presence(status: &tl::enums::UserStatus, now: i64) -> Presence {
    use tl::enums::UserStatus as S;
    match status {
        S::Online(online) if i64::from(online.expires) > now => Presence::Online,
        S::Online(online) => Presence::LastSeen(online.expires.into()),
        S::Offline(offline) => Presence::LastSeen(offline.was_online.into()),
        S::Recently(_) => Presence::Recently,
        _ => Presence::Unknown,
    }
}

fn display_name(user: &User) -> String {
    let name = user.full_name();
    if user.deleted() {
        "Удалённый аккаунт".to_owned()
    } else if name.trim().is_empty() {
        "Без имени".to_owned()
    } else {
        name
    }
}

fn peer_name(peer: &Peer) -> String {
    match peer {
        Peer::User(user) => display_name(user),
        Peer::Group(group) => group.title().unwrap_or("Группа").to_owned(),
        Peer::Channel(channel) => channel.title().to_owned(),
        Peer::Community(community) => community.title().to_owned(),
    }
}

fn chat_info(peer: &Peer, now: i64) -> ChatInfo {
    let mut info = ChatInfo {
        kind: ChatKind::Group,
        title: peer_name(peer),
        presence: Presence::Unknown,
        members: None,
        can_write: true,
    };
    match peer {
        Peer::User(user) if user.is_self() => {
            info.kind = ChatKind::Saved;
            info.title = "Избранное".to_owned();
        }
        Peer::User(user) => {
            info.kind = if user.is_bot() { ChatKind::Bot } else { ChatKind::User };
            info.presence = presence(user.status(), now);
            info.can_write = !user.deleted();
        }
        Peer::Group(group) => {
            info.members = match &group.raw {
                tl::enums::Chat::Chat(chat) => Some(chat.participants_count),
                tl::enums::Chat::Channel(channel) => channel.participants_count,
                _ => None,
            };
        }
        Peer::Channel(channel) => {
            info.kind = ChatKind::Channel;
            info.members = channel.raw.participants_count;
            info.can_write = channel.raw.creator || channel.admin_rights().is_some_and(|rights| rights.post_messages);
        }
        Peer::Community(_) => {}
    }
    info
}

fn media_label(media: &Media) -> String {
    match media {
        Media::Photo(_) => "Фото".to_owned(),
        Media::Sticker(sticker) => format!("Стикер {}", sticker.emoji()).trim_end().to_owned(),
        Media::Document(document) => {
            if document.raw.round {
                "Видеосообщение".to_owned()
            } else if document.raw.voice {
                "Голосовое сообщение".to_owned()
            } else if document.is_animated() {
                "GIF".to_owned()
            } else if document.raw.video {
                "Видео".to_owned()
            } else if let Some(title) = document.audio_title() {
                match document.performer() {
                    Some(performer) => format!("Аудио: {performer} — {title}"),
                    None => format!("Аудио: {title}"),
                }
            } else {
                match document.name() {
                    Some(name) if !name.is_empty() => format!("Файл: {name}"),
                    _ => "Файл".to_owned(),
                }
            }
        }
        Media::Contact(contact) => {
            format!("Контакт: {} {}", contact.first_name(), contact.last_name()).trim_end().to_owned()
        }
        Media::Poll(poll) => {
            let tl::enums::TextWithEntities::Entities(question) = poll.question();
            format!("Опрос: {}", question.text)
        }
        Media::Geo(_) | Media::GeoLive(_) => "Геопозиция".to_owned(),
        Media::Venue(venue) => format!("Место: {}", venue.title()),
        Media::Dice(dice) => format!("{} {}", dice.emoji(), dice.value()),
        // Ссылка уже есть в тексте сообщения.
        Media::WebPage(_) => String::new(),
        _ => "Вложение".to_owned(),
    }
}

/// Текст служебного сообщения: «Анна закрепил(а) сообщение».
fn service_text(message: &Message, action: &tl::enums::MessageAction, me: i64) -> String {
    use tl::enums::MessageAction as A;
    let sender = message.sender_id().map(|id| id.bot_api_dialog_id().unwrap_or(me));
    // Действие от третьего лица и безличная форма для каналов.
    let (did, passive): (String, String) = match action {
        A::ChatCreate(a) => (format!("создал(а) группу «{}»", a.title), format!("Группа «{}» создана", a.title)),
        A::ChannelCreate(a) => (format!("создал(а) «{}»", a.title), format!("Канал «{}» создан", a.title)),
        A::ChatEditTitle(a) => {
            (format!("изменил(а) название на «{}»", a.title), format!("Название изменено на «{}»", a.title))
        }
        A::ChatEditPhoto(_) => ("обновил(а) фото".into(), "Фото обновлено".into()),
        A::ChatDeletePhoto => ("удалил(а) фото".into(), "Фото удалено".into()),
        A::ChatAddUser(a) if a.users.len() == 1 && a.users.first().copied() == sender => {
            ("вступил(а) в группу".into(), "Новый участник".into())
        }
        A::ChatAddUser(_) => ("добавил(а) участников".into(), "Новые участники".into()),
        A::ChatDeleteUser(a) if Some(a.user_id) == sender => ("покинул(а) группу".into(), "Участник вышел".into()),
        A::ChatDeleteUser(_) => ("исключил(а) участника".into(), "Участник исключён".into()),
        A::ChatJoinedByLink(_) | A::ChatJoinedByRequest => ("вступил(а) в группу".into(), "Новый участник".into()),
        A::PinMessage => ("закрепил(а) сообщение".into(), "Сообщение закреплено".into()),
        A::ScreenshotTaken => ("сделал(а) снимок экрана".into(), "Сделан снимок экрана".into()),
        A::TopicCreate(a) => (format!("создал(а) тему «{}»", a.title), format!("Создана тема «{}»", a.title)),
        A::PhoneCall(_) if message.outgoing() => return "Исходящий звонок".to_owned(),
        A::PhoneCall(_) => return "Входящий звонок".to_owned(),
        A::HistoryClear => return "История очищена".to_owned(),
        A::ContactSignUp => return format!("{} теперь в Telegram", message.sender().map(peer_name).unwrap_or_default()),
        _ => return "Служебное сообщение".to_owned(),
    };
    match message.sender().map(peer_name) {
        _ if message.post() => passive,
        _ if message.outgoing() => format!("Вы {}", did.replacen("л(а)", "ли", 1)),
        Some(name) if !name.is_empty() => format!("{name} {did}"),
        _ => passive,
    }
}

fn is_bad_api_id(error: &InvocationError) -> bool {
    error.is("API_ID_INVALID") || error.is("API_ID_PUBLISHED_FLOOD")
}

fn is_unauthorized(error: &InvocationError) -> bool {
    matches!(error, InvocationError::Rpc(rpc) if rpc.code == 401)
}

/// Понятное пользователю описание ошибки запроса.
fn describe(error: &InvocationError) -> String {
    let rpc = match error {
        InvocationError::Rpc(rpc) => rpc,
        InvocationError::Io(_) | InvocationError::Transport(_) => return "Нет связи с Telegram".to_owned(),
        other => return format!("Сбой запроса: {other}"),
    };
    let seconds = rpc.value.unwrap_or_default();
    let text = match rpc.name.as_str() {
        "PHONE_NUMBER_INVALID" => "Неверный номер телефона",
        "PHONE_NUMBER_BANNED" => "Этот номер заблокирован в Telegram",
        "PHONE_NUMBER_FLOOD" | "PHONE_PASSWORD_FLOOD" => "Слишком много попыток входа. Попробуйте позже",
        "PHONE_CODE_EXPIRED" => "Срок действия кода истёк. Запросите новый",
        "AUTH_RESTART" => "Telegram просит начать вход заново",
        "CHAT_WRITE_FORBIDDEN" | "CHAT_SEND_PLAIN_FORBIDDEN" | "CHAT_RESTRICTED" | "CHAT_ADMIN_REQUIRED" => {
            "В этот чат нельзя писать"
        }
        "USER_IS_BLOCKED" | "YOU_BLOCKED_USER" => "Нельзя написать: один из вас заблокировал другого",
        "USER_BANNED_IN_CHANNEL" => "Вам запрещено писать в этом чате",
        "PEER_FLOOD" => "Telegram ограничил отправку сообщений. Попробуйте позже",
        "MESSAGE_TOO_LONG" => "Сообщение слишком длинное",
        "CHANNEL_PRIVATE" => "Чат недоступен",
        "FLOOD_WAIT" | "FLOOD_PREMIUM_WAIT" => return format!("Слишком много запросов. Подождите {seconds} с"),
        "SLOWMODE_WAIT" => return format!("Медленный режим: подождите {seconds} с"),
        name => return format!("Ошибка Telegram: {name} ({})", rpc.code),
    };
    text.to_owned()
}
