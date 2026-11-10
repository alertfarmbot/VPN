//! Каталог данных, учётные данные API и пользовательские настройки.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const CREDENTIALS: &str = "credentials";
const SETTINGS: &str = "settings";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub api_id: i32,
    pub api_hash: String,
}

impl Credentials {
    pub fn parse(api_id: &str, api_hash: &str) -> Option<Self> {
        let api_id: i32 = api_id.trim().parse().ok().filter(|id| *id > 0)?;
        let api_hash = api_hash.trim().to_ascii_lowercase();
        (api_hash.len() == 32 && api_hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(Self { api_id, api_hash })
    }

    pub fn from_env() -> Option<Self> {
        Self::parse(&std::env::var("TG_API_ID").ok()?, &std::env::var("TG_API_HASH").ok()?)
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let values = read_pairs(&dir.join(CREDENTIALS));
        Self::parse(lookup(&values, "api_id")?, lookup(&values, "api_hash")?)
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let text = format!("api_id={}\napi_hash={}\n", self.api_id, self.api_hash);
        write_private(&dir.join(CREDENTIALS), text.as_bytes())
    }

    pub fn forget(dir: &Path) {
        let _ = fs::remove_file(dir.join(CREDENTIALS));
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FERROGRAM_HOME") {
        return PathBuf::from(dir);
    }
    platform_data_dir().unwrap_or_else(|| PathBuf::from(".")).join("ferrogram")
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

/// Создаёт каталог данных. В нём лежит ключ авторизации, поэтому доступ только владельцу.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Атомарно записывает файл с правами только для владельца.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

/// Сохранённая тема: `None`, пока пользователь не выбирал и действует системная.
pub fn load_dark_theme(dir: &Path) -> Option<bool> {
    match lookup(&read_pairs(&dir.join(SETTINGS)), "theme")? {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    }
}

pub fn save_dark_theme(dir: &Path, dark: bool) -> io::Result<()> {
    let text = format!("theme={}\n", if dark { "dark" } else { "light" });
    write_private(&dir.join(SETTINGS), text.as_bytes())
}

fn read_pairs(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn lookup<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_validated() {
        let hash = "0123456789abcdef0123456789ABCDEF";
        let parsed = Credentials::parse(" 12345 ", hash).unwrap();
        assert_eq!(parsed.api_id, 12345);
        assert_eq!(parsed.api_hash, hash.to_ascii_lowercase());
        assert!(Credentials::parse("0", hash).is_none());
        assert!(Credentials::parse("abc", hash).is_none());
        assert!(Credentials::parse("1", "short").is_none());
        assert!(Credentials::parse("1", &"g".repeat(32)).is_none());
    }

    #[test]
    fn credentials_and_theme_round_trip() {
        let dir = std::env::temp_dir().join(format!("ferrogram-config-{}", std::process::id()));
        ensure_dir(&dir).unwrap();
        assert_eq!(Credentials::load(&dir), None);
        assert_eq!(load_dark_theme(&dir), None);

        let creds = Credentials::parse("42", &"ab".repeat(16)).unwrap();
        creds.save(&dir).unwrap();
        assert_eq!(Credentials::load(&dir), Some(creds));
        save_dark_theme(&dir, true).unwrap();
        assert_eq!(load_dark_theme(&dir), Some(true));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(CREDENTIALS)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        Credentials::forget(&dir);
        assert_eq!(Credentials::load(&dir), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
