//! Хранилище сессии MTProto в одном маленьком текстовом файле.
//!
//! Штатное хранилище grammers тянет SQLite. Клиенту нужно пережить перезапуск
//! только с ключами авторизации и сведениями о своём аккаунте, поэтому на диск
//! пишутся только они; кэш собеседников и состояние обновлений живут в памяти.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use grammers_client::session::types::{
    ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_client::session::{BoxFuture, Session, SessionData};

use crate::config::write_private;

const HEADER: &str = "ferrogram-session v1";

pub struct FileSession {
    path: PathBuf,
    state: Mutex<State>,
}

struct State {
    data: SessionData,
    /// Идентификатор своего аккаунта: по нему отвечаем на запрос «себя».
    me: Option<i64>,
}

impl FileSession {
    /// Открывает сессию; отсутствующий или повреждённый файл даёт чистую сессию.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut state = State { data: SessionData::default(), me: None };
        if let Ok(text) = std::fs::read_to_string(&path) {
            parse(&text, &mut state);
        }
        Self { path, state: Mutex::new(state) }
    }

    pub fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| io::Error::other("session lock is poisoned"))
    }

    fn save(&self, state: &State) -> io::Result<()> {
        write_private(&self.path, serialize(state).as_bytes())
    }
}

fn serialize(state: &State) -> String {
    let mut out = format!("{HEADER}\nhome {}\n", state.data.home_dc);
    let mut dcs: Vec<&DcOption> = state.data.dc_options.values().collect();
    dcs.sort_by_key(|dc| dc.id);
    for dc in dcs {
        // Адреса без ключа и так известны из встроенного списка.
        let Some(key) = dc.auth_key else { continue };
        let hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        out.push_str(&format!("dc {} {} {} {hex}\n", dc.id, dc.ipv4, dc.ipv6));
    }
    let me = state.me.and_then(|id| state.data.peer_infos.get(&PeerId::user(id)?));
    if let Some(PeerInfo::User { id, auth, bot, .. }) = me {
        let hash = auth.map(PeerAuth::hash).unwrap_or_default();
        out.push_str(&format!("me {id} {hash} {}\n", u8::from(bot.unwrap_or(false))));
    }
    out
}

fn parse(text: &str, state: &mut State) {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return;
    }
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["home", dc] => {
                if let Ok(dc) = dc.parse() {
                    state.data.home_dc = dc;
                }
            }
            ["dc", id, ipv4, ipv6, key] => {
                let parsed = (id.parse(), ipv4.parse(), ipv6.parse(), parse_key(key));
                if let (Ok(id), Ok(ipv4), Ok(ipv6), Some(key)) = parsed {
                    let dc = DcOption { id, ipv4, ipv6, auth_key: Some(key) };
                    state.data.dc_options.insert(id, dc);
                }
            }
            ["me", id, hash, bot] => {
                let (Ok(id), Ok(hash)) = (id.parse::<i64>(), hash.parse::<i64>()) else { continue };
                let Some(peer) = PeerId::user(id) else { continue };
                let info = PeerInfo::User {
                    id,
                    auth: Some(PeerAuth::from_hash(hash)),
                    bot: Some(*bot == "1"),
                    is_self: Some(true),
                };
                state.data.peer_infos.insert(peer, info);
                state.me = Some(id);
            }
            _ => {}
        }
    }
}

fn parse_key(hex: &str) -> Option<[u8; 256]> {
    if hex.len() != 512 || !hex.is_ascii() {
        return None;
    }
    let mut key = [0u8; 256];
    for (byte, pair) in key.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(key)
}

impl State {
    fn peer(&self, peer: PeerId) -> Option<PeerInfo> {
        // У «себя» особый идентификатор без номера: подставляем настоящий.
        let key = match peer.bot_api_dialog_id() {
            Some(_) => peer,
            None => PeerId::user(self.me?)?,
        };
        self.data.peer_infos.get(&key).cloned()
    }

    /// Возвращает истину, если изменились данные, которые хранятся на диске.
    fn cache(&mut self, peer: PeerInfo) -> bool {
        let is_me = matches!(peer, PeerInfo::User { is_self: Some(true), .. });
        let id = peer.id();
        let before = is_me.then(|| self.data.peer_infos.get(&id).cloned());
        self.data.peer_infos.entry(id).or_insert_with(|| peer.clone()).extend_info(&peer);
        if !is_me {
            return false;
        }
        let PeerInfo::User { id: bare, .. } = peer else { return false };
        let changed = self.me != Some(bare) || before.flatten().as_ref() != self.data.peer_infos.get(&id);
        self.me = Some(bare);
        changed
    }
}

impl Session for FileSession {
    type Error = io::Error;

    fn home_dc_id(&self) -> io::Result<i32> {
        Ok(self.lock()?.data.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let mut state = self.lock()?;
            if state.data.home_dc == dc_id {
                return Ok(());
            }
            state.data.home_dc = dc_id;
            self.save(&state)
        })
    }

    fn dc_option(&self, dc_id: i32) -> io::Result<Option<DcOption>> {
        Ok(self.lock()?.data.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, io::Result<()>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            let mut state = self.lock()?;
            if state.data.dc_options.get(&dc_option.id) == Some(&dc_option) {
                return Ok(());
            }
            state.data.dc_options.insert(dc_option.id, dc_option);
            self.save(&state)
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, io::Result<Option<PeerInfo>>> {
        Box::pin(async move { Ok(self.lock()?.peer(peer)) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, io::Result<()>> {
        self.cache_peers(vec![peer])
    }

    fn cache_peers(&self, peers: Vec<PeerInfo>) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let mut changed = false;
            for peer in peers {
                changed |= state.cache(peer);
            }
            if changed { self.save(&state) } else { Ok(()) }
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, io::Result<UpdatesState>> {
        Box::pin(async move { Ok(self.lock()?.data.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let current = &mut state.data.updates_state;
            match update {
                UpdateState::All(all) => *current = all,
                UpdateState::Primary { pts, date, seq } => {
                    current.pts = pts;
                    current.date = date;
                    current.seq = seq;
                }
                UpdateState::Secondary { qts } => current.qts = qts,
                UpdateState::Channel { id, pts } => {
                    current.channels.retain(|channel| channel.id != id);
                    current.channels.push(ChannelState { id, pts });
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ferrogram-{name}-{}.session", std::process::id()))
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
    }

    fn me(id: i64, hash: i64) -> PeerInfo {
        PeerInfo::User { id, auth: Some(PeerAuth::from_hash(hash)), bot: Some(false), is_self: Some(true) }
    }

    #[test]
    fn auth_data_survives_restart() {
        let path = temp_path("roundtrip");
        FileSession::remove(&path);
        let key = std::array::from_fn(|i| (i * 7 % 251) as u8);

        block_on(async {
            let session = FileSession::open(&path);
            assert_eq!(session.peer(PeerId::self_user()).await.unwrap(), None);
            let mut dc = session.dc_option(4).unwrap().expect("known dc");
            dc.auth_key = Some(key);
            session.set_dc_option(&dc).await.unwrap();
            session.set_home_dc_id(4).await.unwrap();
            session.cache_peer(me(777, -42)).await.unwrap();
            // Обычный собеседник остаётся только в памяти.
            let other = PeerInfo::User { id: 5, auth: Some(PeerAuth::from_hash(9)), bot: None, is_self: None };
            session.cache_peer(other.clone()).await.unwrap();
            assert_eq!(session.peer(PeerId::user(5).unwrap()).await.unwrap(), Some(other));
        });

        let session = FileSession::open(&path);
        assert_eq!(session.home_dc_id().unwrap(), 4);
        assert_eq!(session.dc_option(4).unwrap().unwrap().auth_key, Some(key));
        assert_eq!(session.dc_option(2).unwrap().unwrap().auth_key, None);
        block_on(async {
            assert_eq!(session.peer(PeerId::self_user()).await.unwrap(), Some(me(777, -42)));
            assert_eq!(session.peer(PeerId::user(5).unwrap()).await.unwrap(), None);
        });

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        FileSession::remove(&path);
    }

    #[test]
    fn corrupted_file_gives_clean_session() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "garbage\nhome 5\n").unwrap();
        let session = FileSession::open(&path);
        assert_eq!(session.home_dc_id().unwrap(), SessionData::default().home_dc);

        std::fs::write(&path, format!("{HEADER}\nhome x\ndc 2 nope\nme 1\nhome 3\n")).unwrap();
        let session = FileSession::open(&path);
        assert_eq!(session.home_dc_id().unwrap(), 3);
        assert_eq!(session.dc_option(2).unwrap().unwrap().auth_key, None);
        FileSession::remove(&path);
    }

    #[test]
    fn update_state_is_tracked_in_memory() {
        let path = temp_path("updates");
        let session = FileSession::open(&path);
        block_on(async {
            session.set_update_state(UpdateState::Primary { pts: 10, date: 20, seq: 30 }).await.unwrap();
            session.set_update_state(UpdateState::Channel { id: 1, pts: 5 }).await.unwrap();
            session.set_update_state(UpdateState::Channel { id: 1, pts: 6 }).await.unwrap();
            let state = session.updates_state().await.unwrap();
            assert_eq!((state.pts, state.date, state.seq), (10, 20, 30));
            assert_eq!(state.channels, vec![ChannelState { id: 1, pts: 6 }]);
        });
        assert!(!path.exists());
    }
}
