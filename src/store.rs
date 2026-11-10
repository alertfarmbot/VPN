//! Состояние интерфейса: модели списка чатов и открытой переписки.
//!
//! Живёт в UI-потоке: применяет события воркера к моделям Slint и превращает
//! действия пользователя в команды воркеру.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, FilterModel, Model, ModelRc, SharedString, Timer, VecModel, Weak};
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::{
    AuthState, ChatInfo, ChatKind, Command, DialogData, Event, MessageData, Presence, is_channel_id,
};
use crate::format::{self, Clock};
use crate::{AppWindow, AuthStep, DialogItem, MessageItem, MsgState, config};

/// Ограничение Telegram на длину сообщения в единицах UTF-16.
const MESSAGE_LIMIT: usize = 4096;
/// Сообщения одного автора с паузой меньше этой склеиваются в серию.
const SERIES_GAP: i64 = 600;
const TYPING_SHOWN_FOR: Duration = Duration::from_secs(6);
const TYPING_SENT_EVERY: Duration = Duration::from_secs(4);
const REMOVE_ANIMATION: Duration = Duration::from_millis(240);

thread_local! {
    static STORE: RefCell<Option<Store>> = const { RefCell::new(None) };
}

pub fn install(store: Store) {
    STORE.with(|slot| *slot.borrow_mut() = Some(store));
}

pub fn uninstall() {
    STORE.with(|slot| slot.borrow_mut().take());
}

/// Выполняет действие над состоянием; вызывается только из UI-потока.
pub fn with(action: impl FnOnce(&mut Store)) {
    STORE.with(|slot| {
        // Занято, только если Slint синхронно вызвал нас из нашего же обработчика.
        if let Ok(mut slot) = slot.try_borrow_mut()
            && let Some(store) = slot.as_mut()
        {
            action(store);
        }
    });
}

struct Dialog {
    key: i32,
    info: ChatInfo,
    unread: i32,
    /// Место среди закреплённых; `None` у обычных чатов.
    pin: Option<i64>,
    muted: bool,
    archived: bool,
    read_outbox_max: i32,
    last: Option<MessageData>,
    /// Кто печатает и номер события: по нему таймер узнаёт, что устарел.
    typing: Option<(u64, i64)>,
}

/// То, что нужно знать о строке ленты помимо показанного в ней.
#[derive(Clone, Copy)]
struct Meta {
    id: i32,
    sender: i64,
    date: i64,
    day: i64,
    service: bool,
    outgoing: bool,
}

struct Open {
    id: i64,
    /// Сведения о строках модели сообщений, в том же порядке.
    meta: Vec<Meta>,
    loading: bool,
    reached_start: bool,
    new_below: i32,
    typing_sent: Option<Instant>,
    /// Имена авторов для подписи «… печатает» в группах.
    names: HashMap<i64, String>,
}

pub struct Store {
    ui: Weak<AppWindow>,
    commands: UnboundedSender<Command>,
    clock: Clock,
    dir: PathBuf,
    me: i64,
    dialogs: HashMap<i64, Dialog>,
    /// Идентификаторы чатов в порядке строк модели.
    order: Vec<i64>,
    /// Идентификатор чата по короткому номеру, который знает интерфейс.
    keys: Vec<i64>,
    rows: Rc<VecModel<DialogItem>>,
    query: Rc<RefCell<String>>,
    refilter: Box<dyn Fn()>,
    dialogs_done: bool,
    dialogs_loading: bool,
    open: Option<Open>,
    messages: Rc<VecModel<MessageItem>>,
    drafts: HashMap<i64, SharedString>,
    next_local: i32,
    next_pin: i64,
    top_pin: i64,
    typing_events: u64,
    today: i64,
}

fn first_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

fn same_series(a: &Meta, b: &Meta) -> bool {
    !a.service
        && !b.service
        && a.sender == b.sender
        && a.outgoing == b.outgoing
        && a.day == b.day
        && (b.date - a.date).abs() < SERIES_GAP
}

/// Положение сообщения в серии и заголовок дня — зависят от соседей.
fn links(meta: &[Meta], index: usize, clock: &Clock, now: i64) -> (bool, bool, String) {
    let current = &meta[index];
    let previous = index.checked_sub(1).map(|index| &meta[index]);
    let next = meta.get(index + 1);
    let new_day = previous.is_none_or(|previous| previous.day != current.day);
    let header = if new_day { clock.day_label(current.date, now) } else { String::new() };
    let first = previous.is_none_or(|previous| !same_series(previous, current));
    let last = next.is_none_or(|next| !same_series(current, next));
    (first, last, header)
}

impl Open {
    fn new(id: i64) -> Self {
        Self {
            id,
            meta: Vec::new(),
            loading: false,
            reached_start: false,
            new_below: 0,
            typing_sent: None,
            names: HashMap::new(),
        }
    }

    fn index_of(&self, id: i32) -> Option<usize> {
        self.meta.iter().rposition(|meta| meta.id == id)
    }

    /// Пересчитывает связи строки с соседями и обновляет её, если они изменились.
    fn relink(&self, model: &VecModel<MessageItem>, clock: &Clock, index: usize) {
        if index >= self.meta.len() {
            return;
        }
        let Some(mut row) = model.row_data(index) else { return };
        let (first, last, header) = links(&self.meta, index, clock, Clock::now());
        if row.first != first || row.last != last || row.date_header != header.as_str() {
            row.first = first;
            row.last = last;
            row.date_header = header.into();
            model.set_row_data(index, row);
        }
    }

    fn relink_around(&self, model: &VecModel<MessageItem>, clock: &Clock, index: usize) {
        for index in index.saturating_sub(1)..=index + 1 {
            self.relink(model, clock, index);
        }
    }

    /// Место для сообщения: подтверждённые идут по возрастанию номеров,
    /// отправляемые (с отрицательным номером) остаются в конце.
    fn slot(&self, id: i32) -> usize {
        let mut index = self.meta.len();
        if id > 0 {
            while index > 0 && (self.meta[index - 1].id < 0 || self.meta[index - 1].id > id) {
                index -= 1;
            }
        }
        index
    }

    fn insert(&mut self, model: &VecModel<MessageItem>, clock: &Clock, meta: Meta, row: MessageItem) {
        let index = self.slot(meta.id);
        self.meta.insert(index, meta);
        model.insert(index, row);
        self.relink_around(model, clock, index);
    }

    fn remove(&mut self, model: &VecModel<MessageItem>, clock: &Clock, index: usize) {
        self.meta.remove(index);
        model.remove(index);
        self.relink(model, clock, index);
        if index > 0 {
            self.relink(model, clock, index - 1);
        }
    }
}

impl Store {
    pub fn new(ui: &AppWindow, commands: UnboundedSender<Command>, dir: PathBuf) -> Self {
        let rows = Rc::new(VecModel::<DialogItem>::default());
        let query = Rc::new(RefCell::new(String::new()));
        // Архивные чаты видны только в поиске.
        let filter = Rc::new(FilterModel::new(rows.clone(), {
            let query = query.clone();
            move |row: &DialogItem| {
                let query = query.borrow();
                if query.is_empty() { !row.archived } else { row.title.to_lowercase().contains(query.as_str()) }
            }
        }));
        ui.set_dialogs(ModelRc::from(filter.clone()));
        let messages = Rc::new(VecModel::<MessageItem>::default());
        ui.set_messages(ModelRc::from(messages.clone()));

        let clock = Clock::system();
        let today = clock.day(Clock::now());
        Self {
            ui: ui.as_weak(),
            commands,
            clock,
            dir,
            me: 0,
            dialogs: HashMap::new(),
            order: Vec::new(),
            keys: Vec::new(),
            rows,
            query,
            refilter: Box::new(move || filter.reset()),
            dialogs_done: false,
            dialogs_loading: false,
            open: None,
            messages,
            drafts: HashMap::new(),
            next_local: -1,
            next_pin: 0,
            top_pin: -1,
            typing_events: 0,
            today,
        }
    }

    fn send(&self, command: Command) {
        // Воркера уже нет только при закрытии приложения.
        let _ = self.commands.send(command);
    }

    // ----- Вход -----

    fn submit(&self, command: Command) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_auth_busy(true);
            ui.set_auth_error(SharedString::new());
        }
        self.send(command);
    }

    pub fn submit_credentials(&self, api_id: &str, api_hash: &str) {
        self.submit(Command::SetCredentials { api_id: api_id.to_owned(), api_hash: api_hash.to_owned() });
    }

    pub fn submit_phone(&self, phone: &str) {
        self.submit(Command::SubmitPhone(phone.to_owned()));
    }

    pub fn submit_code(&self, code: &str) {
        self.submit(Command::SubmitCode(code.to_owned()));
    }

    pub fn submit_password(&self, password: &str) {
        self.submit(Command::SubmitPassword(password.to_owned()));
    }

    pub fn auth_back(&self) {
        self.send(Command::AuthBack);
    }

    pub fn logout(&self) {
        self.send(Command::Logout);
    }

    pub fn theme_toggled(&self, dark: bool) {
        let _ = config::ensure_dir(&self.dir).and_then(|()| config::save_dark_theme(&self.dir, dark));
    }

    // ----- Список чатов -----

    fn dialog_row(&self, id: i64, dialog: &Dialog, now: i64) -> DialogItem {
        let kind = dialog.info.kind;
        let last = dialog.last.as_ref().filter(|message| !message.service);
        let prefix = match last {
            Some(message) if kind != ChatKind::Group => {
                debug_assert_eq!(message.chat, id);
                String::new()
            }
            Some(message) if message.outgoing => "Вы: ".to_owned(),
            Some(message) if !message.sender.is_empty() => format!("{}: ", first_word(&message.sender)),
            _ => String::new(),
        };
        let state = match last {
            Some(message) if message.outgoing && matches!(kind, ChatKind::User | ChatKind::Bot | ChatKind::Group) => {
                if message.id < 0 {
                    MsgState::Pending
                } else if message.id <= dialog.read_outbox_max {
                    MsgState::Read
                } else {
                    MsgState::Sent
                }
            }
            _ => MsgState::None,
        };
        DialogItem {
            key: dialog.key,
            title: dialog.info.title.as_str().into(),
            initials: format::initials(&dialog.info.title).into(),
            color: format::color_index(id),
            saved: kind == ChatKind::Saved,
            prefix: prefix.into(),
            preview: dialog.last.as_ref().map(format::preview).unwrap_or_default().into(),
            time: dialog.last.as_ref().map(|message| self.clock.list_stamp(message.date, now)).unwrap_or_default().into(),
            unread: dialog.unread,
            muted: dialog.muted,
            pinned: dialog.pin.is_some(),
            online: dialog.info.presence == Presence::Online,
            typing: dialog.typing.is_some(),
            archived: dialog.archived,
            state,
        }
    }

    /// Закреплённые сверху в своём порядке, остальные — по времени последнего сообщения.
    fn sort_key(dialog: &Dialog, id: i64) -> (bool, i64, i64, i64) {
        let date = dialog.last.as_ref().map_or(0, |message| message.date);
        (dialog.pin.is_none(), dialog.pin.unwrap_or(0), -date, id)
    }

    /// Обновляет строку чата и переставляет её, если изменилось место в списке.
    fn place(&mut self, id: i64) {
        let Some(dialog) = self.dialogs.get(&id) else { return };
        let row = self.dialog_row(id, dialog, Clock::now());
        let key = Self::sort_key(dialog, id);
        let old = self.order.iter().position(|other| *other == id);
        let others = self.order.iter().filter(|other| **other != id);
        let target = others
            .clone()
            .position(|other| self.dialogs.get(other).is_none_or(|dialog| key < Self::sort_key(dialog, *other)))
            .unwrap_or(others.count());
        match old {
            Some(index) if index == target => self.rows.set_row_data(index, row),
            Some(index) => {
                self.order.remove(index);
                self.rows.remove(index);
                self.order.insert(target, id);
                self.rows.insert(target, row);
            }
            None => {
                self.order.insert(target, id);
                self.rows.insert(target, row);
            }
        }
    }

    fn add_dialog(&mut self, data: DialogData) {
        let id = data.id;
        let known = self.dialogs.get(&id);
        let pin = match (data.pinned, known.and_then(|dialog| dialog.pin)) {
            (false, _) => None,
            (true, Some(pin)) => Some(pin),
            (true, None) => {
                self.next_pin += 1;
                Some(self.next_pin)
            }
        };
        let key = known.map(|dialog| dialog.key).unwrap_or_else(|| {
            self.keys.push(id);
            (self.keys.len() - 1) as i32
        });
        let typing = known.and_then(|dialog| dialog.typing);
        self.dialogs.insert(
            id,
            Dialog {
                key,
                info: data.info,
                unread: data.unread,
                pin,
                muted: data.muted,
                archived: data.archived,
                read_outbox_max: data.read_outbox_max,
                last: data.last,
                typing,
            },
        );
        self.place(id);
    }

    pub fn load_more_dialogs(&mut self) {
        if self.me == 0 || self.dialogs_done || self.dialogs_loading {
            return;
        }
        self.dialogs_loading = true;
        self.send(Command::LoadDialogs);
    }

    pub fn search_edited(&mut self, text: &str) {
        *self.query.borrow_mut() = text.trim().to_lowercase();
        (self.refilter)();
        // Искать можно только среди загруженных чатов, поэтому догружаем остальные.
        if !self.query.borrow().is_empty() {
            self.load_more_dialogs();
        }
    }

    // ----- Открытый чат -----

    fn is_open(&self, chat: i64) -> bool {
        self.open.as_ref().is_some_and(|open| open.id == chat)
    }

    fn remember_name(&mut self, message: &MessageData) {
        if let Some(open) = &mut self.open
            && !message.sender.is_empty()
            && !open.names.contains_key(&message.sender_id)
        {
            open.names.insert(message.sender_id, message.sender.clone());
        }
    }

    fn refresh_header(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(open) = &self.open else { return };
        let Some(dialog) = self.dialogs.get(&open.id) else { return };
        let info = &dialog.info;
        let typing = dialog.typing.map(|(_, user)| match (info.kind, open.names.get(&user)) {
            (ChatKind::Group, Some(name)) => format!("{} печатает", first_word(name)),
            _ => "печатает".to_owned(),
        });
        let members = |one, few, many, fallback: &str| match info.members {
            Some(count) if count > 0 => {
                format!("{} {}", format::count(count.into()), format::plural(count.into(), one, few, many))
            }
            _ => fallback.to_owned(),
        };
        let status = match info.kind {
            ChatKind::Saved => String::new(),
            ChatKind::Bot => "бот".to_owned(),
            ChatKind::Group => members("участник", "участника", "участников", "группа"),
            ChatKind::Channel => members("подписчик", "подписчика", "подписчиков", "канал"),
            ChatKind::User => match info.presence {
                Presence::Online => "в сети".to_owned(),
                Presence::LastSeen(at) => self.clock.last_seen(at, Clock::now()),
                Presence::Recently => "был(а) недавно".to_owned(),
                Presence::Unknown => "был(а) давно".to_owned(),
            },
        };
        ui.set_chat_title(info.title.as_str().into());
        ui.set_chat_initials(format::initials(&info.title).into());
        ui.set_chat_color(format::color_index(open.id));
        ui.set_chat_saved(info.kind == ChatKind::Saved);
        ui.set_chat_group(info.kind == ChatKind::Group);
        ui.set_chat_can_write(info.can_write);
        ui.set_chat_typing(typing.is_some());
        ui.set_chat_subtitle(typing.unwrap_or(status).into());
    }

    fn stash_draft(&mut self, ui: &AppWindow) {
        if let Some(open) = &self.open {
            let draft = ui.get_draft();
            if draft.is_empty() {
                self.drafts.remove(&open.id);
            } else {
                self.drafts.insert(open.id, draft);
            }
        }
    }

    pub fn open_chat(&mut self, key: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(&id) = usize::try_from(key).ok().and_then(|key| self.keys.get(key)) else { return };
        if self.is_open(id) {
            ui.invoke_focus_composer();
            return;
        }
        self.stash_draft(&ui);
        self.messages.set_vec(Vec::new());
        self.open = Some(Open { loading: true, ..Open::new(id) });
        ui.set_draft(self.drafts.get(&id).cloned().unwrap_or_default());
        ui.set_current_key(key);
        ui.set_new_below(0);
        ui.set_history_loading(true);
        ui.set_chat_open(true);
        ui.invoke_jump_to_bottom();
        self.refresh_header();
        ui.invoke_focus_composer();
        self.send(Command::LoadHistory { chat: id, before: None });
        self.mark_read(id);
    }

    pub fn close_chat(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        self.stash_draft(&ui);
        self.open = None;
        ui.set_chat_open(false);
        ui.set_current_key(-1);
        ui.set_draft(SharedString::new());
        // Ленту очищаем после того, как панель уедет, иначе она опустеет на глазах.
        Timer::single_shot(Duration::from_millis(280), || {
            with(|store| {
                if store.open.is_none() {
                    store.messages.set_vec(Vec::new());
                }
            });
        });
    }

    fn mark_read(&mut self, chat: i64) {
        if let Some(dialog) = self.dialogs.get_mut(&chat)
            && dialog.unread > 0
        {
            dialog.unread = 0;
            self.send(Command::MarkRead(chat));
            self.place(chat);
        }
    }

    /// Пользователь смотрит на конец открытого чата: всё прочитано.
    pub fn chat_viewed(&mut self) {
        let Some(open) = &mut self.open else { return };
        open.new_below = 0;
        let chat = open.id;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_new_below(0);
        }
        self.mark_read(chat);
    }

    pub fn need_older(&mut self) {
        let Some(open) = &mut self.open else { return };
        if open.loading || open.reached_start {
            return;
        }
        let Some(oldest) = open.meta.iter().find(|meta| meta.id > 0) else { return };
        let command = Command::LoadHistory { chat: open.id, before: Some(oldest.id) };
        open.loading = true;
        self.send(command);
    }

    pub fn send_message(&mut self, text: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(chat) = self.open.as_ref().map(|open| open.id) else { return };
        let parts = format::split_message(text, MESSAGE_LIMIT);
        if parts.is_empty() {
            return;
        }
        for part in parts {
            let message = MessageData {
                id: self.next_local,
                chat,
                date: Clock::now(),
                outgoing: true,
                sender_id: self.me,
                sender: String::new(),
                text: part,
                media: String::new(),
                service: false,
                edited: false,
            };
            self.next_local -= 1;
            let (meta, row) = message_row(&self.clock, &message, self.dialogs.get(&chat), true);
            if let Some(open) = &mut self.open {
                open.insert(&self.messages, &self.clock, meta, row);
            }
            self.send(Command::Send { chat, text: message.text.clone(), local_id: message.id });
            if let Some(dialog) = self.dialogs.get_mut(&chat) {
                dialog.last = Some(message);
            }
        }
        ui.invoke_scroll_to_bottom();
        self.place(chat);
    }

    pub fn retry_message(&mut self, id: i32) {
        let Some(open) = &self.open else { return };
        let Some(index) = open.index_of(id).filter(|_| id < 0) else { return };
        let Some(mut row) = self.messages.row_data(index) else { return };
        if row.state != MsgState::Failed {
            return;
        }
        row.state = MsgState::Pending;
        row.fresh = false;
        let command = Command::Send { chat: open.id, text: row.text.to_string(), local_id: id };
        self.messages.set_row_data(index, row);
        self.send(command);
    }

    pub fn composer_edited(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(open) = &mut self.open else { return };
        let kind = self.dialogs.get(&open.id).map(|dialog| dialog.info.kind);
        let due = open.typing_sent.is_none_or(|at| at.elapsed() >= TYPING_SENT_EVERY);
        if due && matches!(kind, Some(ChatKind::User | ChatKind::Group)) && !ui.get_draft().is_empty() {
            open.typing_sent = Some(Instant::now());
            let command = Command::Typing(open.id);
            self.send(command);
        }
    }

    /// Добавляет в открытую ленту сообщение, пришедшее обновлением.
    fn append_live(&mut self, ui: &AppWindow, message: &MessageData) {
        let dialog = self.dialogs.get(&message.chat);
        let read_max = dialog.map_or(0, |dialog| dialog.read_outbox_max);
        let Some(open) = &mut self.open else { return };
        if open.index_of(message.id).is_some() {
            return;
        }
        // Своё сообщение может прийти обновлением раньше ответа на отправку:
        // тогда оно подтверждает строку, которая ещё «отправляется».
        if message.outgoing {
            let pending = open.meta.iter().position(|meta| meta.id < 0).and_then(|start| {
                (start..open.meta.len()).find(|index| {
                    open.meta[*index].id < 0
                        && self.messages.row_data(*index).is_some_and(|row| row.text == message.text.as_str())
                })
            });
            if let Some(index) = pending {
                return confirm(open, &self.messages, &self.clock, index, message, read_max);
            }
        }
        let at_bottom = ui.get_at_bottom();
        if !at_bottom {
            ui.set_keep_position(true);
            if !message.outgoing {
                open.new_below += 1;
                ui.set_new_below(open.new_below);
            }
        }
        let (meta, row) = message_row(&self.clock, message, dialog, at_bottom);
        if let Some(open) = &mut self.open {
            open.insert(&self.messages, &self.clock, meta, row);
        }
    }

    fn purge_removed(&mut self, chat: i64) {
        let Some(open) = self.open.as_mut().filter(|open| open.id == chat) else { return };
        for index in (0..open.meta.len()).rev() {
            if self.messages.row_data(index).is_some_and(|row| row.removing) {
                open.remove(&self.messages, &self.clock, index);
            }
        }
    }

    fn typing_expired(&mut self, chat: i64, event: u64) {
        if let Some(dialog) = self.dialogs.get_mut(&chat)
            && dialog.typing.is_some_and(|(current, _)| current == event)
        {
            dialog.typing = None;
            self.place(chat);
            if self.is_open(chat) {
                self.refresh_header();
            }
        }
    }

    /// Раз в минуту: с наступлением нового дня подписи времени устаревают.
    pub fn tick(&mut self) {
        let now = Clock::now();
        let today = self.clock.day(now);
        if today == self.today {
            return;
        }
        self.today = today;
        for (index, id) in self.order.iter().enumerate() {
            if let Some(dialog) = self.dialogs.get(id) {
                self.rows.set_row_data(index, self.dialog_row(*id, dialog, now));
            }
        }
        if let Some(open) = &self.open {
            for index in 0..open.meta.len() {
                open.relink(&self.messages, &self.clock, index);
            }
        }
        self.refresh_header();
    }

    fn reset(&mut self, ui: &AppWindow) {
        self.me = 0;
        self.dialogs.clear();
        self.order.clear();
        self.keys.clear();
        self.drafts.clear();
        self.rows.set_vec(Vec::new());
        self.messages.set_vec(Vec::new());
        self.open = None;
        self.dialogs_done = false;
        self.dialogs_loading = false;
        self.query.borrow_mut().clear();
        (self.refilter)();
        ui.set_chat_open(false);
        ui.set_current_key(-1);
        ui.set_draft(SharedString::new());
        ui.set_online(true);
    }

    // ----- События воркера -----

    pub fn apply(&mut self, event: Event) {
        let Some(ui) = self.ui.upgrade() else { return };
        match event {
            Event::Auth(state) => {
                ui.set_auth_busy(false);
                ui.set_auth_error(SharedString::new());
                ui.set_auth_step(match state {
                    AuthState::Connecting => AuthStep::Connecting,
                    AuthState::NeedCredentials => AuthStep::Credentials,
                    AuthState::NeedPhone => AuthStep::Phone,
                    AuthState::NeedCode { phone } => {
                        ui.set_auth_phone(phone.into());
                        AuthStep::Code
                    }
                    AuthState::NeedPassword { hint } => {
                        ui.set_password_hint(hint.into());
                        AuthStep::Password
                    }
                });
            }
            Event::AuthError(text) => {
                ui.set_auth_busy(false);
                ui.set_auth_error(text.into());
            }
            Event::Ready { me, name } => {
                self.reset(&ui);
                self.me = me;
                self.dialogs_loading = true;
                ui.set_me_initials(format::initials(&name).into());
                ui.set_me_color(format::color_index(me));
                ui.set_me_name(name.into());
                ui.set_dialogs_loading(true);
                ui.set_auth_busy(false);
                ui.set_authorized(true);
            }
            Event::LoggedOut => {
                self.reset(&ui);
                ui.set_auth_step(AuthStep::Connecting);
                ui.set_auth_error(SharedString::new());
                ui.set_authorized(false);
            }
            Event::Connection(online) => ui.set_online(online),
            Event::Dialogs { items, done } => {
                for item in items {
                    self.add_dialog(item);
                }
                self.dialogs_done = done;
                self.dialogs_loading = false;
                ui.set_dialogs_loading(false);
                if !self.query.borrow().is_empty() {
                    self.load_more_dialogs();
                }
            }
            Event::History { chat, messages, older, end } => self.history(&ui, chat, messages, older, end),
            Event::HistoryFailed { chat } => {
                if let Some(open) = self.open.as_mut().filter(|open| open.id == chat) {
                    open.loading = false;
                    ui.set_history_loading(false);
                }
            }
            Event::NewMessage { message, info } => self.new_message(&ui, message, info),
            Event::MessageEdited(message) => self.edited(message),
            Event::MessagesDeleted { chat, ids } => self.deleted(chat, &ids),
            Event::Preview { chat, last } => {
                if let Some(dialog) = self.dialogs.get_mut(&chat) {
                    dialog.last = last;
                    self.place(chat);
                }
            }
            Event::Sent { chat, local_id, message } => self.sent(chat, local_id, message),
            Event::SendFailed { chat, local_id } => {
                if let Some(open) = self.open.as_ref().filter(|open| open.id == chat)
                    && let Some(index) = open.index_of(local_id)
                    && let Some(mut row) = self.messages.row_data(index)
                {
                    row.state = MsgState::Failed;
                    row.fresh = false;
                    self.messages.set_row_data(index, row);
                }
                // В списке чатов снова показываем последнее настоящее сообщение.
                self.send(Command::RefreshPreview(chat));
            }
            Event::ReadOutbox { chat, max_id } => self.read_outbox(chat, max_id),
            Event::ReadInbox { chat, unread } => {
                if let Some(dialog) = self.dialogs.get_mut(&chat) {
                    dialog.unread = unread;
                    self.place(chat);
                }
            }
            Event::Typing { chat, user } => {
                let Some(dialog) = self.dialogs.get_mut(&chat) else { return };
                self.typing_events += 1;
                let event = self.typing_events;
                dialog.typing = Some((event, user));
                self.place(chat);
                if self.is_open(chat) {
                    self.refresh_header();
                }
                Timer::single_shot(TYPING_SHOWN_FOR, move || with(|store| store.typing_expired(chat, event)));
            }
            Event::Presence { user, presence } => {
                if let Some(dialog) = self.dialogs.get_mut(&user) {
                    dialog.info.presence = presence;
                    self.place(user);
                    if self.is_open(user) {
                        self.refresh_header();
                    }
                }
            }
            Event::Pinned { chat, pinned } => {
                if let Some(dialog) = self.dialogs.get_mut(&chat) {
                    // Только что закреплённый чат встаёт первым.
                    dialog.pin = pinned.then_some(self.top_pin);
                    self.top_pin -= 1;
                    self.place(chat);
                }
            }
            Event::Muted { chat, muted } => {
                if let Some(dialog) = self.dialogs.get_mut(&chat) {
                    dialog.muted = muted;
                    self.place(chat);
                }
            }
            Event::Error(text) => ui.invoke_show_toast(text.into()),
            Event::Fatal(text) => ui.set_fatal(text.into()),
        }
    }

    fn history(&mut self, ui: &AppWindow, chat: i64, messages: Vec<MessageData>, older: bool, end: bool) {
        if !self.is_open(chat) {
            return;
        }
        for message in &messages {
            self.remember_name(message);
        }
        let dialog = self.dialogs.get(&chat);
        let mut incoming: Vec<(Meta, MessageItem)> =
            messages.iter().map(|message| message_row(&self.clock, message, dialog, false)).collect();
        let Some(open) = &mut self.open else { return };
        open.loading = false;
        open.reached_start |= end;
        ui.set_history_loading(false);

        if older {
            let oldest = open.meta.iter().find(|meta| meta.id > 0).map(|meta| meta.id);
            incoming.retain(|(meta, _)| oldest.is_none_or(|oldest| meta.id < oldest));
            let added = incoming.len();
            for (meta, row) in incoming.into_iter().rev() {
                open.meta.insert(0, meta);
                self.messages.insert(0, row);
            }
            for index in 0..=added {
                open.relink(&self.messages, &self.clock, index);
            }
            return;
        }

        // Первая страница. То, что успело появиться раньше неё (отправляемые
        // и более новые сообщения), остаётся в конце ленты.
        let newest = incoming.last().map_or(0, |(meta, _)| meta.id);
        for (index, meta) in open.meta.iter().enumerate() {
            if (meta.id < 0 || meta.id > newest)
                && let Some(mut row) = self.messages.row_data(index)
            {
                row.fresh = false;
                incoming.push((*meta, row));
            }
        }
        let (meta, mut rows): (Vec<Meta>, Vec<MessageItem>) = incoming.into_iter().unzip();
        let now = Clock::now();
        for (index, row) in rows.iter_mut().enumerate() {
            let (first, last, header) = links(&meta, index, &self.clock, now);
            row.first = first;
            row.last = last;
            row.date_header = header.into();
        }
        open.meta = meta;
        self.messages.set_vec(rows);
        ui.invoke_jump_to_bottom();
    }

    fn new_message(&mut self, ui: &AppWindow, message: MessageData, info: ChatInfo) {
        let chat = message.chat;
        if !self.dialogs.contains_key(&chat) {
            self.add_dialog(DialogData {
                id: chat,
                info,
                unread: 0,
                pinned: false,
                muted: false,
                archived: false,
                read_outbox_max: 0,
                last: None,
            });
        }
        let open = self.is_open(chat);
        let viewing = open && ui.get_viewing();
        let Some(dialog) = self.dialogs.get_mut(&chat) else { return };
        let typing_ended = dialog.typing.take().is_some();
        if dialog.last.as_ref().is_none_or(|last| last.id < 0 || message.id >= last.id) {
            dialog.last = Some(message.clone());
        }
        if message.outgoing {
            dialog.unread = 0;
        } else if viewing {
            self.send(Command::MarkRead(chat));
        } else {
            dialog.unread += 1;
        }
        self.place(chat);
        if open {
            self.remember_name(&message);
            self.append_live(ui, &message);
            if typing_ended {
                self.refresh_header();
            }
        }
    }

    fn sent(&mut self, chat: i64, local_id: i32, message: Option<MessageData>) {
        if let Some(dialog) = self.dialogs.get_mut(&chat) {
            match &message {
                Some(message) => {
                    if dialog.last.as_ref().is_none_or(|last| last.id < 0 || message.id >= last.id) {
                        dialog.last = Some(message.clone());
                    }
                    self.place(chat);
                }
                None => self.send(Command::RefreshPreview(chat)),
            }
        }
        let read_max = self.dialogs.get(&chat).map_or(0, |dialog| dialog.read_outbox_max);
        let dialog = self.dialogs.get(&chat);
        let Some(open) = self.open.as_mut().filter(|open| open.id == chat) else { return };
        match (open.index_of(local_id), message) {
            // Сообщение уже пришло обновлением и стоит в ленте.
            (Some(index), Some(message)) if open.index_of(message.id).is_some() => {
                open.remove(&self.messages, &self.clock, index);
            }
            (Some(index), Some(message)) => confirm(open, &self.messages, &self.clock, index, &message, read_max),
            (Some(index), None) => {
                if let Some(mut row) = self.messages.row_data(index) {
                    row.state = MsgState::Sent;
                    row.fresh = false;
                    self.messages.set_row_data(index, row);
                }
            }
            (None, Some(message)) if open.index_of(message.id).is_none() => {
                let (meta, row) = message_row(&self.clock, &message, dialog, false);
                open.insert(&self.messages, &self.clock, meta, row);
            }
            (None, _) => {}
        }
    }

    fn edited(&mut self, message: MessageData) {
        let chat = message.chat;
        if let Some(dialog) = self.dialogs.get_mut(&chat)
            && dialog.last.as_ref().is_some_and(|last| last.id == message.id)
        {
            dialog.last = Some(message.clone());
            self.place(chat);
        }
        if let Some(open) = self.open.as_ref().filter(|open| open.id == chat)
            && let Some(index) = open.index_of(message.id)
            && let Some(mut row) = self.messages.row_data(index)
        {
            row.text = message.text.as_str().into();
            row.media = message.media.as_str().into();
            row.edited = message.edited;
            row.fresh = false;
            self.messages.set_row_data(index, row);
        }
    }

    fn deleted(&mut self, chat: Option<i64>, ids: &[i32]) {
        // Без указания чата удаление относится к личным чатам и обычным группам.
        let affects = |id: i64| chat.map_or(!is_channel_id(id), |chat| chat == id);
        let stale: Vec<i64> = self
            .dialogs
            .iter()
            .filter(|(id, dialog)| affects(**id) && dialog.last.as_ref().is_some_and(|last| ids.contains(&last.id)))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.send(Command::RefreshPreview(id));
        }
        let Some(open) = self.open.as_ref().filter(|open| affects(open.id)) else { return };
        let mut found = false;
        for id in ids {
            if let Some(index) = open.index_of(*id)
                && let Some(mut row) = self.messages.row_data(index)
            {
                row.removing = true;
                self.messages.set_row_data(index, row);
                found = true;
            }
        }
        if found {
            let chat = open.id;
            Timer::single_shot(REMOVE_ANIMATION, move || with(|store| store.purge_removed(chat)));
        }
    }

    fn read_outbox(&mut self, chat: i64, max_id: i32) {
        let Some(dialog) = self.dialogs.get_mut(&chat) else { return };
        dialog.read_outbox_max = dialog.read_outbox_max.max(max_id);
        self.place(chat);
        let Some(open) = self.open.as_ref().filter(|open| open.id == chat) else { return };
        for (index, meta) in open.meta.iter().enumerate().rev() {
            if meta.id < 0 || meta.id > max_id || !meta.outgoing {
                continue;
            }
            let Some(mut row) = self.messages.row_data(index) else { continue };
            match row.state {
                MsgState::Sent => {
                    row.state = MsgState::Read;
                    row.fresh = false;
                    self.messages.set_row_data(index, row);
                }
                // Всё, что раньше, уже отмечено.
                MsgState::Read => break,
                _ => {}
            }
        }
    }
}

/// Строит строку ленты и сведения о ней. `fresh` включает анимацию появления.
fn message_row(clock: &Clock, message: &MessageData, dialog: Option<&Dialog>, fresh: bool) -> (Meta, MessageItem) {
    let kind = dialog.map_or(ChatKind::User, |dialog| dialog.info.kind);
    // В каналах все записи идут единой лентой слева, без галочек.
    let outgoing = message.outgoing && kind != ChatKind::Channel;
    let state = if !outgoing || message.service || kind == ChatKind::Saved {
        MsgState::None
    } else if message.id < 0 {
        MsgState::Pending
    } else if message.id <= dialog.map_or(0, |dialog| dialog.read_outbox_max) {
        MsgState::Read
    } else {
        MsgState::Sent
    };
    let meta = Meta {
        id: message.id,
        sender: message.sender_id,
        date: message.date,
        day: clock.day(message.date),
        service: message.service,
        outgoing,
    };
    let row = MessageItem {
        id: message.id,
        text: message.text.as_str().into(),
        media: message.media.as_str().into(),
        time: clock.time(message.date).into(),
        sender: message.sender.as_str().into(),
        sender_initials: format::initials(&message.sender).into(),
        sender_color: format::color_index(message.sender_id),
        date_header: SharedString::new(),
        outgoing,
        service: message.service,
        edited: message.edited,
        first: true,
        last: true,
        state,
        fresh,
        removing: false,
    };
    (meta, row)
}

/// Превращает «отправляемую» строку в подтверждённое сообщение, не пересоздавая её.
fn confirm(
    open: &mut Open,
    model: &VecModel<MessageItem>,
    clock: &Clock,
    index: usize,
    message: &MessageData,
    read_max: i32,
) {
    let Some(mut row) = model.row_data(index) else { return };
    let meta = &mut open.meta[index];
    meta.id = message.id;
    meta.date = message.date;
    meta.day = clock.day(message.date);
    row.id = message.id;
    row.time = clock.time(message.date).into();
    row.fresh = false;
    if row.state != MsgState::None {
        row.state = if message.id <= read_max { MsgState::Read } else { MsgState::Sent };
    }
    model.set_row_data(index, row);
    open.relink_around(model, clock, index);
}
