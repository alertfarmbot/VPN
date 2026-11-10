//! Демо-режим: тот же поток событий, что и у настоящего клиента, но без сети.
//!
//! Нужен, чтобы посмотреть интерфейс без учётных данных Telegram.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::{JoinSet, LocalSet};
use tokio::time::sleep;

use super::{
    AuthState, ChatInfo, ChatKind, Command, DialogData, Emit, Event, MessageData, Presence,
};
use crate::format::Clock;

const ME: i64 = 1000;
const ANNA: i64 = 101;
const IGOR: i64 = 102;
const MARIA: i64 = 103;
const TEAM: i64 = -201;
const NEWS: i64 = -1_000_000_000_301;
const PAGE: usize = 50;

const REPLIES: [&str; 5] = [
    "Отлично, договорились!",
    "Сейчас посмотрю и отвечу подробнее.",
    "Хм, интересная мысль. А если сделать ещё проще?",
    "Да, вижу. Работает быстро и памяти почти не ест 🙂",
    "Принято. Напишу, когда будет готово.",
];

type Commands = UnboundedReceiver<Command>;

struct State {
    histories: HashMap<i64, Vec<MessageData>>,
    dialogs: Vec<DialogData>,
    next_id: i32,
    replies: usize,
}

pub fn run(commands: Commands, emit: Emit, mut with_login: bool) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_time().build() else {
        emit(Event::Fatal("Не удалось запустить демо-режим".into()));
        return;
    };
    LocalSet::new().block_on(&runtime, async move {
        let mut commands = commands;
        loop {
            if with_login && !login(&mut commands, &emit).await {
                return;
            }
            // После выхода из аккаунта показываем экран входа.
            with_login = true;
            if !session(&mut commands, &emit).await {
                return;
            }
        }
    });
}

/// Проходит шаги входа; ложь означает, что приложение закрывается.
async fn login(commands: &mut Commands, emit: &Emit) -> bool {
    emit(Event::Auth(AuthState::NeedPhone));
    loop {
        let Some(command) = commands.recv().await else { return false };
        match command {
            Command::Quit => return false,
            Command::AuthBack => emit(Event::Auth(AuthState::NeedPhone)),
            Command::SubmitPhone(phone) => {
                sleep(Duration::from_millis(500)).await;
                emit(Event::Auth(AuthState::NeedCode { phone }));
            }
            Command::SubmitCode(code) => {
                sleep(Duration::from_millis(500)).await;
                if code.trim() == "00000" {
                    emit(Event::AuthError("Неверный код".into()));
                } else {
                    emit(Event::Auth(AuthState::NeedPassword { hint: "в демо подойдёт любой".into() }));
                }
            }
            Command::SubmitPassword(_) => {
                sleep(Duration::from_millis(500)).await;
                return true;
            }
            _ => {}
        }
    }
}

/// Работает до выхода из аккаунта (истина) или закрытия приложения (ложь).
async fn session(commands: &mut Commands, emit: &Emit) -> bool {
    let state = Rc::new(RefCell::new(State::new(Clock::now())));
    let mut tasks = JoinSet::new();
    emit(Event::Ready { me: ME, name: "Демо-аккаунт".into() });
    tasks.spawn_local(intro(state.clone(), emit.clone()));
    loop {
        let Some(command) = commands.recv().await else { return false };
        match command {
            Command::Quit => return false,
            Command::Logout => {
                emit(Event::LoggedOut);
                return true;
            }
            Command::LoadHistory { chat, before } => {
                tasks.spawn_local(history(state.clone(), emit.clone(), chat, before));
            }
            Command::Send { chat, text, local_id } => {
                tasks.spawn_local(send(state.clone(), emit.clone(), chat, text, local_id));
            }
            Command::RefreshPreview(chat) => {
                let last = state.borrow().histories.get(&chat).and_then(|history| history.last().cloned());
                emit(Event::Preview { chat, last });
            }
            _ => {}
        }
        while tasks.try_join_next().is_some() {}
    }
}

/// Отдаёт список чатов и чуть позже разыгрывает входящее сообщение.
async fn intro(state: Rc<RefCell<State>>, emit: Emit) {
    sleep(Duration::from_millis(700)).await;
    let items = state.borrow().dialogs.clone();
    emit(Event::Dialogs { items, done: true });

    sleep(Duration::from_secs(5)).await;
    emit(Event::Typing { chat: ANNA, user: ANNA });
    sleep(Duration::from_secs(3)).await;
    let message = state.borrow_mut().push(ANNA, ANNA, "Анна Смирнова", false, "Кстати, вечером созвонимся?");
    emit(Event::NewMessage { message, info: user("Анна Смирнова", Presence::Online) });
}

async fn history(state: Rc<RefCell<State>>, emit: Emit, chat: i64, before: Option<i32>) {
    sleep(Duration::from_millis(250)).await;
    let (messages, end) = {
        let state = state.borrow();
        let all = state.histories.get(&chat).map(Vec::as_slice).unwrap_or_default();
        let upto = before.map_or(all.len(), |id| all.partition_point(|message| message.id < id));
        let from = upto.saturating_sub(PAGE);
        (all[from..upto].to_vec(), from == 0)
    };
    emit(Event::History { chat, messages, older: before.is_some(), end });
}

async fn send(state: Rc<RefCell<State>>, emit: Emit, chat: i64, text: String, local_id: i32) {
    sleep(Duration::from_millis(450)).await;
    if text.trim() == "/fail" {
        emit(Event::Error("В этот чат нельзя писать".into()));
        emit(Event::SendFailed { chat, local_id });
        return;
    }
    let message = state.borrow_mut().push(chat, ME, "", true, &text);
    let sent_id = message.id;
    emit(Event::Sent { chat, local_id, message: Some(message) });
    if chat == ME || chat == NEWS {
        return;
    }

    sleep(Duration::from_millis(900)).await;
    emit(Event::ReadOutbox { chat, max_id: sent_id });
    let (sender, name) = if chat == TEAM { (IGOR, "Игорь Петров") } else { (chat, "") };
    sleep(Duration::from_millis(600)).await;
    emit(Event::Typing { chat, user: sender });
    sleep(Duration::from_millis(2200)).await;
    let (message, info) = {
        let mut state = state.borrow_mut();
        let reply = REPLIES[state.replies % REPLIES.len()];
        state.replies += 1;
        let Some(info) = state.dialogs.iter().find(|dialog| dialog.id == chat).map(|dialog| dialog.info.clone())
        else {
            return;
        };
        (state.push(chat, sender, name, false, reply), info)
    };
    emit(Event::NewMessage { message, info });
}

fn user(title: &str, presence: Presence) -> ChatInfo {
    ChatInfo { kind: ChatKind::User, title: title.into(), presence, members: None, can_write: true }
}

fn group(title: &str, members: i32) -> ChatInfo {
    ChatInfo {
        kind: ChatKind::Group,
        title: title.into(),
        presence: Presence::Unknown,
        members: Some(members),
        can_write: true,
    }
}

impl State {
    fn push(&mut self, chat: i64, sender: i64, name: &str, outgoing: bool, text: &str) -> MessageData {
        let message = MessageData {
            id: self.next_id,
            chat,
            date: Clock::now(),
            outgoing,
            sender_id: sender,
            sender: name.to_owned(),
            text: text.to_owned(),
            media: String::new(),
            service: false,
            edited: false,
        };
        self.next_id += 1;
        self.histories.entry(chat).or_default().push(message.clone());
        message
    }

    fn new(now: i64) -> Self {
        let mut state = Self { histories: HashMap::new(), dialogs: Vec::new(), next_id: 1, replies: 0 };
        const MIN: i64 = 60;
        const HOUR: i64 = 3600;
        const DAY: i64 = 86_400;

        // (отправитель, имя, текст); пустое имя — исходящее сообщение.
        let anna: &[(i64, &str, &str)] = &[
            (ANNA, "Анна Смирнова", "Привет! Ты уже пробовал новый клиент?"),
            (ME, "", "Привет! Да, как раз сижу в нём 🙂"),
            (ANNA, "Анна Смирнова", "И как впечатления?"),
            (
                ME,
                "",
                "Запускается мгновенно и занимает считаные мегабайты памяти. Интерфейс рисуется \
                 на процессоре, без видеокарты, а анимации всё равно плавные.",
            ),
            (ANNA, "Анна Смирнова", "Звучит здорово.\nА переписка подгружается при прокрутке?"),
            (ME, "", "Да, по пятьдесят сообщений за раз."),
            (ANNA, "Анна Смирнова", "Тогда я тоже хочу попробовать!"),
            (ANNA, "Анна Смирнова", "Пришлёшь ссылку на сборку?"),
        ];
        state.fill(ANNA, now - 40 * MIN, 4 * MIN, anna);
        state.dialog(ANNA, user("Анна Смирнова", Presence::Online), 2, |_| {});

        let phrases: [(i64, &str, &str); 9] = [
            (IGOR, "Игорь Петров", "Коллеги, сборка прошла, все тесты зелёные."),
            (MARIA, "Мария Ким", "Отлично! Я обновила макеты экрана входа."),
            (ME, "", "Посмотрю после обеда."),
            (ANNA, "Анна Смирнова", "Напоминаю: демо в пятницу в 15:00."),
            (IGOR, "Игорь Петров", "Замерил память: клиент занимает меньше, чем одна вкладка браузера."),
            (MARIA, "Мария Ким", "А анимации списка чатов уже готовы?"),
            (ME, "", "Да, и появление сообщений тоже."),
            (IGOR, "Игорь Петров", "Тогда осталось описать установку в README."),
            (ANNA, "Анна Смирнова", "Беру на себя 👍"),
        ];
        let team: Vec<(i64, &str, &str)> = phrases.iter().cycle().take(130).copied().collect();
        state.fill(TEAM, now - 3 * DAY, 33 * MIN, &team);
        if let Some(history) = state.histories.get_mut(&TEAM) {
            history[100].service = true;
            history[100].text = "Мария Ким закрепил(а) сообщение".into();
            history[118].edited = true;
            history[124].media = "Файл: отчёт-о-памяти.pdf".into();
        }
        state.dialog(TEAM, group("Команда проекта", 5), 5, |_| {});

        state.fill(
            NEWS,
            now - 2 * DAY,
            20 * HOUR,
            &[
                (NEWS, "Новости Rust", "Вышла новая стабильная версия компилятора: быстрее сборка и новые линты."),
                (NEWS, "Новости Rust", "Подборка недели: десять небольших библиотек для настольных приложений."),
                (NEWS, "Новости Rust", "Как мы уменьшили потребление памяти в три раза"),
            ],
        );
        if let Some(last) = state.histories.get_mut(&NEWS).and_then(|history| history.last_mut()) {
            last.media = "Фото".into();
        }
        let news = ChatInfo {
            kind: ChatKind::Channel,
            title: "Новости Rust".into(),
            presence: Presence::Unknown,
            members: Some(12_840),
            can_write: false,
        };
        state.dialog(NEWS, news, 3, |dialog| dialog.muted = true);

        state.fill(ME, now - 5 * HOUR, HOUR, &[(ME, "", "Список покупок: молоко, хлеб, кофе")]);
        let saved = ChatInfo { kind: ChatKind::Saved, ..user("Избранное", Presence::Unknown) };
        state.dialog(ME, saved, 0, |dialog| dialog.pinned = true);

        let simple: [(i64, &str, Presence, i64, &[(bool, &str)]); 9] = [
            (104, "Дмитрий Орлов", Presence::LastSeen(now - 26 * HOUR), now - 27 * HOUR, &[
                (false, "Документы отправил на почту."),
                (true, "Получил, спасибо!"),
            ]),
            (105, "Елена Васильева", Presence::Recently, now - 2 * DAY, &[
                (true, "Лена, удобно будет созвониться завтра в одиннадцать?"),
            ]),
            (106, "Алексей Волков", Presence::LastSeen(now - 9 * DAY), now - 9 * DAY, &[
                (false, "Спасибо за помощь с настройкой!"),
            ]),
            (107, "Ольга Белова", Presence::Online, now - 3 * HOUR, &[
                (false, "Фотографии с выходных получились отличные"),
            ]),
            (108, "Сергей Никитин", Presence::Recently, now - 4 * DAY, &[(true, "Ок, до связи")]),
            (109, "Наталья Зайцева", Presence::LastSeen(now - 50 * MIN), now - 12 * DAY, &[
                (false, "С днём рождения! 🎂"),
                (true, "Спасибо большое!"),
            ]),
            (110, "Павел Морозов", Presence::Unknown, now - 20 * DAY, &[(false, "Перезвоню позже")]),
            (111, "Виктория Лебедева", Presence::Recently, now - 45 * DAY, &[
                (true, "Договорились, тогда до встречи"),
            ]),
            (112, "Михаил Соколов", Presence::LastSeen(now - 400 * DAY), now - 400 * DAY, &[
                (false, "Давно не виделись!"),
            ]),
        ];
        for (id, name, presence, at, lines) in simple {
            let lines: Vec<(i64, &str, &str)> =
                lines.iter().map(|(out, text)| if *out { (ME, "", *text) } else { (id, name, *text) }).collect();
            state.fill(id, at, 5 * MIN, &lines);
            state.dialog(id, user(name, presence), 0, |dialog| {
                // У Дмитрия ответ уже прочитан, у остальных исходящие ещё нет.
                if id == 104 {
                    dialog.read_outbox_max = i32::MAX;
                }
            });
        }
        if let Some(history) = state.histories.get_mut(&107) {
            history[0].media = "Фото".into();
        }

        state.fill(113, now - 6 * HOUR, MIN, &[(113, "Погода", "Сегодня ясно, +18°, ветер слабый.")]);
        let bot = ChatInfo { kind: ChatKind::Bot, ..user("Погода", Presence::Unknown) };
        state.dialog(113, bot, 1, |dialog| dialog.muted = true);

        state.fill(-202, now - 8 * HOUR, 10 * MIN, &[
            (120, "Мама", "Не забудьте про ужин в воскресенье"),
            (ME, "", "Будем к шести"),
            (121, "Папа", "Я куплю торт"),
        ]);
        state.dialog(-202, group("Семья", 4), 0, |_| {});

        state.fill(-203, now - 5 * DAY, 3 * HOUR, &[
            (122, "Зоя", "В этом месяце читаем что-нибудь из классики. Предложения?"),
            (123, "Тимофей", "Голосую за сборник рассказов."),
        ]);
        state.dialog(-203, group("Книжный клуб", 28), 0, |_| {});

        state.dialogs.sort_by_key(|dialog| {
            (!dialog.pinned, -dialog.last.as_ref().map_or(0, |message| message.date))
        });
        state
    }

    /// Добавляет переписку: сообщения идут с шагом `step`, начиная с `start`.
    fn fill(&mut self, chat: i64, start: i64, step: i64, lines: &[(i64, &str, &str)]) {
        for (index, (sender, name, text)) in lines.iter().enumerate() {
            let message = self.push(chat, *sender, name, *sender == ME, text);
            if let Some(stored) = self.histories.get_mut(&chat).and_then(|history| history.last_mut()) {
                stored.date = start + step * index as i64;
                debug_assert_eq!(stored.id, message.id);
            }
        }
    }

    fn dialog(&mut self, id: i64, info: ChatInfo, unread: i32, adjust: impl FnOnce(&mut DialogData)) {
        let mut dialog = DialogData {
            id,
            info,
            unread,
            pinned: false,
            muted: false,
            archived: false,
            read_outbox_max: 0,
            last: self.histories.get(&id).and_then(|history| history.last().cloned()),
        };
        adjust(&mut dialog);
        self.dialogs.push(dialog);
    }
}
