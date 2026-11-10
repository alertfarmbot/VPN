//! Сообщения между UI-потоком и сетевым воркером.
//!
//! Воркер ничего не знает про интерфейс: он принимает [`Command`] и отдаёт
//! [`Event`] через [`Emit`]. Демо-режим реализует тот же контракт без сети.

pub mod demo;
pub mod telegram;

use std::sync::Arc;

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    User,
    Bot,
    Saved,
    Group,
    Channel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Unknown,
    Online,
    /// Время последнего визита, unix-секунды.
    LastSeen(i64),
    Recently,
}

#[derive(Debug, Clone)]
pub struct ChatInfo {
    pub kind: ChatKind,
    pub title: String,
    pub presence: Presence,
    pub members: Option<i32>,
    /// Ложь для каналов, куда нельзя писать.
    pub can_write: bool,
}

#[derive(Debug, Clone)]
pub struct DialogData {
    pub id: i64,
    pub info: ChatInfo,
    pub unread: i32,
    pub pinned: bool,
    pub muted: bool,
    pub archived: bool,
    pub read_outbox_max: i32,
    pub last: Option<MessageData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageData {
    /// Идентификатор в чате; отрицательный у ещё не отправленных.
    pub id: i32,
    pub chat: i64,
    pub date: i64,
    pub outgoing: bool,
    pub sender_id: i64,
    pub sender: String,
    pub text: String,
    /// Подпись вложения («Фото», «Файл: …»); сами файлы не скачиваются.
    pub media: String,
    pub service: bool,
    pub edited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    Connecting,
    NeedCredentials,
    NeedPhone,
    NeedCode { phone: String },
    NeedPassword { hint: String },
}

#[derive(Debug)]
pub enum Event {
    Auth(AuthState),
    /// Ошибка на текущем шаге входа; шаг остаётся прежним.
    AuthError(String),
    Ready { me: i64, name: String },
    LoggedOut,
    Connection(bool),
    Dialogs { items: Vec<DialogData>, done: bool },
    History { chat: i64, messages: Vec<MessageData>, older: bool, end: bool },
    HistoryFailed { chat: i64 },
    NewMessage { message: MessageData, info: ChatInfo },
    MessageEdited(MessageData),
    /// `chat` известен только для каналов и супергрупп: у личных чатов и
    /// обычных групп общее пространство идентификаторов сообщений.
    MessagesDeleted { chat: Option<i64>, ids: Vec<i32> },
    Preview { chat: i64, last: Option<MessageData> },
    Sent { chat: i64, local_id: i32, message: Option<MessageData> },
    SendFailed { chat: i64, local_id: i32 },
    ReadOutbox { chat: i64, max_id: i32 },
    ReadInbox { chat: i64, unread: i32 },
    Typing { chat: i64, user: i64 },
    Presence { user: i64, presence: Presence },
    Pinned { chat: i64, pinned: bool },
    Muted { chat: i64, muted: bool },
    Error(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum Command {
    SetCredentials { api_id: String, api_hash: String },
    SubmitPhone(String),
    SubmitCode(String),
    SubmitPassword(String),
    AuthBack,
    LoadDialogs,
    LoadHistory { chat: i64, before: Option<i32> },
    RefreshPreview(i64),
    Send { chat: i64, text: String, local_id: i32 },
    MarkRead(i64),
    Typing(i64),
    Logout,
    Quit,
}

/// Идентификаторы каналов и супергрупп лежат ниже этой границы.
pub fn is_channel_id(id: i64) -> bool {
    id <= -1_000_000_000_000
}
