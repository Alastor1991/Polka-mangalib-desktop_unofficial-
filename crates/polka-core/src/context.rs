//! Ядро целиком: его поднимает оболочка при запуске и раздаёт экранам.

use anyhow::Result;
use libsocial::{BookmarkFolder, Client, Site};

use crate::catalog::Constants;
use crate::config::Config;
use crate::db::Db;
use crate::downloads::Downloader;
use crate::inbox::Inbox;
use crate::library::Library;
use crate::secrets::TokenStore;
use crate::session;
use crate::sync::Sync;

/// Всё, что нужно экранам для работы. Клонируется дёшево: внутри общие ссылки.
#[derive(Clone)]
pub struct Core {
    pub client: Client,
    pub db: Db,
    pub library: Library,
    pub downloader: Downloader,
    pub sync: Sync,
    /// Счётчики уведомлений и сообщений с фоновым опросом.
    pub inbox: Inbox,
    pub config: Config,
    pub store: TokenStore,
    /// Вошедший пользователь. `None` у гостя и пока ответ сервера не пришёл.
    pub user_id: Option<u64>,
    /// Папки закладок пользователя. Пусто у гостя и без сети.
    pub folders: Vec<BookmarkFolder>,
    /// Справочник формы каталога, общий для всех экранов.
    pub constants: Constants,
}

impl Core {
    /// Поднимает ядро: сохранённый вход, база, загрузчик, отправка отметок, опрос
    /// уведомлений.
    /// Вызывать в рантайме tokio, который живёт всё время работы приложения:
    /// на нём крутятся загрузчик, отправка и опрос.
    pub async fn start(config: Config) -> Result<Core> {
        let (client, store) = session::connect(&config).await?;
        let db = Db::open(&config.db_path())?;
        let library = Library::new(config.library_dir());
        let downloader = Downloader::start(
            client.clone(),
            db.clone(),
            library.clone(),
            config.download_concurrency,
            config.image_server.clone(),
            &config.lock_path(),
        );
        let sync = Sync::start(client.clone(), db.clone());
        let inbox = Inbox::start(client.clone());
        Ok(Core {
            client,
            db,
            library,
            downloader,
            sync,
            inbox,
            config,
            store,
            user_id: None,
            folders: Vec::new(),
            constants: Constants::new(),
        })
    }

    /// Ядро для тестов: база в памяти, библиотека во временном каталоге, сервер
    /// по адресу `api` (обычно мок-сервер), вход не выполнен.
    #[cfg(test)]
    pub fn for_tests(dir: &std::path::Path, api: &str) -> Core {
        let config = Config {
            data_dir: Some(dir.to_path_buf()),
            api_mirrors: vec![api.to_string()],
            ..Config::default()
        };
        let client = Client::new(libsocial::ClientConfig {
            api_mirrors: config.api_mirrors.clone(),
            api_interval: std::time::Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        let db = Db::in_memory().unwrap();
        let library = Library::new(config.library_dir());
        let downloader = Downloader::start(
            client.clone(),
            db.clone(),
            library.clone(),
            1,
            "main".into(),
            &dir.join("downloader.lock"),
        );
        Core {
            sync: Sync::detached(db.clone()),
            inbox: Inbox::detached(),
            client,
            db,
            library,
            downloader,
            config,
            store: TokenStore::File(dir.join("token.json")),
            user_id: None,
            folders: Vec::new(),
            constants: Constants::new(),
        }
    }

    /// Папки закладок для сайта в порядке сервера.
    pub fn folders_for(&self, site: Site) -> Vec<&BookmarkFolder> {
        self.folders.iter().filter(|f| f.is_for(site)).collect()
    }
}
