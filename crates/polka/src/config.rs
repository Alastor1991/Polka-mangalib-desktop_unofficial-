//! Пути и настройки. Каталоги считаются по XDG вручную, без лишних зависимостей.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP: &str = "polka";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
        .join(APP)
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

pub fn default_data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Сетевой интерфейс для запросов к lib.social, например `enp4s0`.
    pub interface: Option<String>,
    /// Прокси для запросов к lib.social, например `socks5h://127.0.0.1:1080`.
    pub proxy: Option<String>,
    /// Свои зеркала API вместо встроенных, например `["https://api.cdnlibs.org/api"]`.
    pub api_mirrors: Vec<String>,
    /// Свои серверы авторизации вместо встроенных.
    pub auth_mirrors: Vec<String>,
    /// Где хранить библиотеку. По умолчанию `~/.local/share/polka`.
    pub data_dir: Option<PathBuf>,
    /// Предпочтительный сервер картинок: `compress`, `main`, `secondary`, `download`.
    pub image_server: String,
    /// Предел качества видео, по умолчанию 1080.
    pub max_quality: u64,
    /// Сколько картинок качать одновременно.
    pub download_concurrency: usize,
    /// Предел дискового кэша обложек и страниц, мегабайты.
    pub cache_limit_mb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interface: None,
            proxy: None,
            api_mirrors: Vec::new(),
            auth_mirrors: Vec::new(),
            data_dir: None,
            image_server: "compress".into(),
            max_quality: 1080,
            download_concurrency: 4,
            cache_limit_mb: 1024,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Читает настройки. Переменные `POLKA_INTERFACE` и `POLKA_PROXY` перекрывают файл.
    pub fn load() -> Result<Config> {
        let mut config = Self::load_file()?;
        if let Ok(iface) = std::env::var("POLKA_INTERFACE") {
            config.interface = Some(iface).filter(|s| !s.is_empty());
        }
        if let Ok(proxy) = std::env::var("POLKA_PROXY") {
            config.proxy = Some(proxy).filter(|s| !s.is_empty());
        }
        Ok(config)
    }

    /// Только файл настроек, без переменных окружения. Его и надо сохранять,
    /// иначе прокси с паролем из окружения навсегда осел бы в файле.
    pub fn load_file() -> Result<Config> {
        let path = Self::path();
        let mut config: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("не разобрать {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("не прочитать {}", path.display())),
        };
        config.download_concurrency = config.download_concurrency.clamp(1, 8);
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        write_atomic(&path, toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(default_data_dir)
    }

    pub fn library_dir(&self) -> PathBuf {
        self.data_dir().join("library")
    }

    /// Файл с зеркалом API, которое сработало в прошлый раз.
    pub fn last_mirror_path(&self) -> PathBuf {
        cache_dir().join("api-mirror")
    }

    /// Блокировка загрузчика: качает только процесс, который её держит.
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir().join("downloader.lock")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("polka.sqlite3")
    }

    pub fn client_config(&self) -> libsocial::ClientConfig {
        libsocial::ClientConfig {
            interface: self.interface.clone(),
            proxy: self.proxy.clone(),
            api_mirrors: self.api_mirrors.clone(),
            auth_mirrors: self.auth_mirrors.clone(),
            ..Default::default()
        }
    }
}

/// Пишет файл через временный рядом, чтобы обрыв не оставил половину.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("у пути нет каталога")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
