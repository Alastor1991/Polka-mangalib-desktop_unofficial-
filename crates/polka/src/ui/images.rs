//! Картинки для интерфейса: обложки и страницы глав.
//!
//! В памяти лежат картинки, уже размещённые в видеопамяти, на диске – кэш
//! скачанного из сети в `~/.cache/polka/images`. Сеть ограничена семафором, чтобы
//! читалка не открыла полсотни соединений к CDN разом.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;
use iced::widget::image::{Allocation, Handle};
use libsocial::models::{PageImage, image_format};
use libsocial::{Client, Site};
use tokio::sync::Semaphore;

/// Сколько обложек держать в видеопамяти. Старые выгружаются первыми.
const COVER_LIMIT: usize = 400;

/// Что загрузить.
#[derive(Debug, Clone)]
pub enum ImageReq {
    /// Обложка или другая картинка по прямому адресу.
    Url { site: Site, url: String },
    /// Страница главы из сети: адрес собирается из справочника серверов.
    Page {
        site: Site,
        page: PageImage,
        server: String,
    },
    /// Страница скачанной главы.
    LocalPage(PathBuf),
    /// Обложка на диске, рядом со скачанными главами.
    File(PathBuf),
}

impl ImageReq {
    pub fn key(&self) -> String {
        match self {
            ImageReq::Url { url, .. } => format!("url:{url}"),
            ImageReq::Page { page, .. } => format!("page:{}", page.url),
            ImageReq::LocalPage(path) => format!("lpage:{}", path.display()),
            ImageReq::File(path) => format!("file:{}", path.display()),
        }
    }
}

fn is_page_key(key: &str) -> bool {
    key.starts_with("page:") || key.starts_with("lpage:")
}

#[derive(Debug, Clone)]
pub enum Loaded {
    /// Картинка прочитана, но ещё не размещена в видеопамяти.
    Ok(Handle),
    /// Размещена: размер известен, отрисуется в следующем кадре.
    Allocated(Allocation),
    Failed(String),
}

pub struct Images {
    client: Client,
    cache_dir: PathBuf,
    net: Arc<Semaphore>,
    /// Размещённые картинки. В iced 0.14 размер неразмещённой картинки неизвестен,
    /// и картинка шириной во всё окно получила бы нулевую высоту, поэтому
    /// показываем только размещённые.
    allocs: HashMap<String, Allocation>,
    /// Обложки в порядке размещения, для выгрузки старых.
    covers: VecDeque<String>,
    /// Ожидаемые результаты. Пришедшее не отсюда (страница закрытой главы)
    /// выбрасывается, а не оседает в памяти.
    pending: HashSet<String>,
    failed: HashMap<String, String>,
}

impl Images {
    pub fn new(client: Client, cache_dir: PathBuf, concurrency: usize) -> Images {
        Images {
            client,
            cache_dir,
            net: Arc::new(Semaphore::new(concurrency.max(1))),
            allocs: HashMap::new(),
            covers: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Handle> {
        self.allocs.get(key).map(Allocation::handle)
    }

    pub fn error(&self, key: &str) -> Option<&str> {
        self.failed.get(key).map(String::as_str)
    }

    /// Запускает загрузку, если картинки ещё нет и она не грузится.
    pub fn request<M: Send + 'static>(&mut self, req: ImageReq, done: fn(String, Loaded) -> M) -> Task<M> {
        let key = req.key();
        // Упавшую картинку сами не перезапрашиваем: иначе каждый кадр читалки
        // бил бы по серверу. Повтор только явно, через `retry` или `forget_failed`.
        if self.allocs.contains_key(&key) || self.pending.contains(&key) || self.failed.contains_key(&key) {
            return Task::none();
        }
        self.pending.insert(key.clone());
        if let ImageReq::File(path) | ImageReq::LocalPage(path) = &req {
            // С диска сеть не нужна: сразу размещаем.
            return allocate(key, Handle::from_path(path), done);
        }
        let client = self.client.clone();
        let cache = self.cache_dir.clone();
        let net = self.net.clone();
        Task::perform(load(client, cache, net, req), move |result| done(key.clone(), result))
    }

    /// Повторить неудавшуюся загрузку.
    pub fn retry<M: Send + 'static>(&mut self, req: ImageReq, done: fn(String, Loaded) -> M) -> Task<M> {
        self.failed.remove(&req.key());
        self.request(req, done)
    }

    /// Разрешает снова грузить упавшие обложки: например, после смены вкладки.
    pub fn forget_failed(&mut self) {
        self.failed.retain(|k, _| is_page_key(k));
    }

    /// Принимает результат загрузки. Прочитанную картинку отправляет размещаться,
    /// поэтому может вернуть ещё одну задачу.
    pub fn finish<M: Send + 'static>(&mut self, key: String, loaded: Loaded, done: fn(String, Loaded) -> M) -> Task<M> {
        if std::env::var_os("POLKA_DEBUG").is_some() {
            let what = match &loaded {
                Loaded::Ok(_) => "прочитана".to_string(),
                Loaded::Allocated(a) => format!("размещена {:?}", a.size()),
                Loaded::Failed(e) => e.clone(),
            };
            eprintln!("картинка {}: {what}", short_key(&key));
        }
        if !self.pending.contains(&key) {
            // Картинку уже не ждут: глава закрыта или кэш страниц сброшен.
            return Task::none();
        }
        match loaded {
            Loaded::Ok(handle) => allocate(key, handle, done),
            Loaded::Allocated(allocation) => {
                self.pending.remove(&key);
                if key.starts_with("url:") || key.starts_with("file:") {
                    self.covers.push_back(key.clone());
                    while self.covers.len() > COVER_LIMIT {
                        if let Some(old) = self.covers.pop_front() {
                            self.allocs.remove(&old);
                        }
                    }
                }
                self.allocs.insert(key, allocation);
                Task::none()
            }
            Loaded::Failed(e) => {
                self.pending.remove(&key);
                // Испорченный файл в кэше иначе отдавался бы снова и снова.
                let _ = std::fs::remove_file(cache_file(&self.cache_dir, &key));
                self.failed.insert(key, e);
                Task::none()
            }
        }
    }

    /// Выкидывает из памяти страницы глав и забывает их ожидание. Обложки остаются.
    pub fn forget_pages(&mut self) {
        self.allocs.retain(|k, _| !is_page_key(k));
        self.pending.retain(|k| !is_page_key(k));
        self.failed.retain(|k, _| !is_page_key(k));
    }
}

/// Ключ для журнала отладки: без полного адреса и пути.
fn short_key(key: &str) -> String {
    let (kind, rest) = key.split_once(':').unwrap_or(("?", key));
    let tail: String = rest.rsplit('/').next().unwrap_or(rest).chars().take(40).collect();
    format!("{kind}:…/{tail}")
}

fn allocate<M: Send + 'static>(key: String, handle: Handle, done: fn(String, Loaded) -> M) -> Task<M> {
    iced::widget::image::allocate(handle).map(move |result| {
        let loaded = match result {
            Ok(allocation) => Loaded::Allocated(allocation),
            Err(e) => Loaded::Failed(format!("картинка не открылась: {e}")),
        };
        done(key.clone(), loaded)
    })
}

fn cache_file(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir.join(format!("{:016x}", hash(key)))
}

async fn load(client: Client, cache_dir: PathBuf, net: Arc<Semaphore>, req: ImageReq) -> Loaded {
    let cache_file = cache_file(&cache_dir, &req.key());
    if let Ok(bytes) = tokio::fs::read(&cache_file).await {
        // Обновляем время, по нему чистится кэш: свежие файлы живут дольше.
        let _ = filetime_touch(&cache_file);
        return Loaded::Ok(Handle::from_bytes(bytes));
    }
    let Ok(_permit) = net.acquire().await else {
        return Loaded::Failed("загрузчик картинок остановлен".into());
    };
    let result = match &req {
        ImageReq::Url { site, url } => client.fetch_bytes(*site, url).await,
        ImageReq::Page { site, page, server } => client.fetch_page(*site, page, server).await,
        ImageReq::File(_) | ImageReq::LocalPage(_) => unreachable!("файлы читаются без сети"),
    };
    match result {
        // Страница блокировки со статусом 200 в кэш не попадает.
        Ok(bytes) if image_format(&bytes).is_none() => Loaded::Failed("сервер вернул не картинку".into()),
        Ok(bytes) => {
            if tokio::fs::create_dir_all(&cache_dir).await.is_ok() {
                let tmp = cache_file.with_extension("part");
                if tokio::fs::write(&tmp, &bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &cache_file).await;
                }
            }
            Loaded::Ok(Handle::from_bytes(bytes))
        }
        Err(e) => Loaded::Failed(e.to_string()),
    }
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn filetime_touch(path: &Path) -> std::io::Result<()> {
    let file = std::fs::File::options().append(true).open(path)?;
    file.set_modified(std::time::SystemTime::now())
}

/// Чистит дисковый кэш до предела, удаляя самые давно использованные файлы.
/// Трогает только файлы в своём каталоге кэша.
pub fn prune_cache(cache_dir: &Path, limit_bytes: u64) -> std::io::Result<u64> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Ok(0);
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    let mut freed = 0;
    for (_, size, path) in files {
        if total <= limit_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            freed += size;
        }
    }
    Ok(freed)
}
