//! Хранение токена входа. Основное место – системное хранилище секретов (Secret Service:
//! gnome-keyring, KWallet). Если его нет, токен лежит в файле с правами 0600.

use std::path::PathBuf;

use anyhow::{Context, Result};
use libsocial::Token;

use crate::config;

const SERVICE: &str = "polka";
const ACCOUNT: &str = "lib.social";

#[derive(Debug, Clone)]
pub enum TokenStore {
    Keyring,
    File(PathBuf),
}

impl TokenStore {
    /// Выбирает хранилище: системное, если оно отвечает, иначе файл.
    pub fn detect() -> TokenStore {
        let keyring_ok = keyring::Entry::store_status().is_ok() && keyring::Entry::new(SERVICE, ACCOUNT).is_ok();
        if keyring_ok {
            TokenStore::Keyring
        } else {
            TokenStore::File(config::config_dir().join("token.json"))
        }
    }

    pub fn describe(&self) -> String {
        match self {
            TokenStore::Keyring => "системное хранилище секретов".into(),
            TokenStore::File(path) => format!("файл {}", path.display()),
        }
    }

    pub fn load(&self) -> Result<Option<Token>> {
        let json = match self {
            TokenStore::Keyring => match keyring::Entry::new(SERVICE, ACCOUNT)?.get_password() {
                Ok(json) => json,
                Err(keyring::Error::NoEntry) => return Ok(None),
                Err(e) => return Err(e).context("хранилище секретов не отдало токен"),
            },
            TokenStore::File(path) => match std::fs::read_to_string(path) {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("не прочитать {}", path.display())),
            },
        };
        Ok(serde_json::from_str(&json).ok())
    }

    pub fn save(&self, token: Option<&Token>) -> Result<()> {
        match (self, token) {
            (TokenStore::Keyring, Some(token)) => {
                keyring::Entry::new(SERVICE, ACCOUNT)?.set_password(&serde_json::to_string(token)?)?;
            }
            (TokenStore::Keyring, None) => match keyring::Entry::new(SERVICE, ACCOUNT)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => return Err(e.into()),
            },
            (TokenStore::File(path), Some(token)) => write_private(path, &serde_json::to_vec(token)?)?,
            (TokenStore::File(path), None) => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }
}

/// Файл, который может прочитать только владелец.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().context("у пути нет каталога")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
