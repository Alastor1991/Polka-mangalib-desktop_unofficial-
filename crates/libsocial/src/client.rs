use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, ORIGIN, REFERER};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::auth::Token;
use crate::error::{Error, Result};
use crate::mirrors;
use crate::models::{Constants, Data};
use crate::site::Site;

/// Сколько раз повторять запрос при 429, 5xx и обрыве сети.
const RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Зеркала API в порядке предпочтения. Пустой список – встроенные.
    pub api_mirrors: Vec<String>,
    /// Серверы авторизации. Пустой список – встроенные.
    pub auth_mirrors: Vec<String>,
    pub user_agent: String,
    /// Сетевой интерфейс для всех запросов (только Linux). Нужен, когда системный
    /// VPN выводит трафик туда, откуда сервис недоступен.
    pub interface: Option<String>,
    /// Прокси вида `socks5h://127.0.0.1:1080` или `http://...`.
    pub proxy: Option<String>,
    /// Пауза между запросами к API.
    pub api_interval: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_mirrors: Vec::new(),
            auth_mirrors: Vec::new(),
            user_agent: format!("Polka/{} (unofficial lib.social client)", env!("CARGO_PKG_VERSION")),
            interface: None,
            proxy: None,
            api_interval: Duration::from_millis(500),
        }
    }
}

/// Вызывается, когда клиент получил или обновил токен, чтобы приложение его сохранило.
pub type TokenListener = Arc<dyn Fn(Option<&Token>) + Send + Sync>;

/// Клиент API. Дёшево клонируется, все клоны делят токен, кэш, зеркала и очередь.
#[derive(Clone)]
pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) config: Arc<ClientConfig>,
    api_mirrors: Arc<Vec<String>>,
    auth_mirrors: Arc<Vec<String>>,
    active_api: Arc<AtomicUsize>,
    limiter: Arc<Mutex<Instant>>,
    pub(crate) token: Arc<RwLock<Option<Token>>>,
    refresh_lock: Arc<Mutex<()>>,
    listener: Option<TokenListener>,
    pub(crate) constants: Arc<RwLock<Option<Constants>>>,
    /// Хосты картинок и видео, которые недавно не ответили. Их пробуем последними.
    bad_hosts: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("api", &self.api_base())
            .finish_non_exhaustive()
    }
}

/// Как отправлять запрос к API.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    /// С токеном, если он есть, и с обновлением токена при 401.
    User,
    /// Без токена: обмен кода и обновление самого токена.
    None,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Client> {
        let mut builder = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120));
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
        if let Some(iface) = &config.interface {
            builder = builder.interface(iface);
        }
        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        Ok(Client {
            http: builder.build()?,
            api_mirrors: Arc::new(mirrors::or_default(&config.api_mirrors, mirrors::API)),
            auth_mirrors: Arc::new(mirrors::or_default(&config.auth_mirrors, mirrors::AUTH)),
            config: Arc::new(config),
            active_api: Arc::new(AtomicUsize::new(0)),
            limiter: Arc::new(Mutex::new(Instant::now())),
            token: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
            listener: None,
            constants: Arc::new(RwLock::new(None)),
            bad_hosts: Arc::new(std::sync::Mutex::new(HashSet::new())),
        })
    }

    pub fn with_token_listener(mut self, listener: TokenListener) -> Client {
        self.listener = Some(listener);
        self
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Зеркало API, через которое клиент сейчас работает.
    pub fn api_base(&self) -> String {
        let idx = self.active_api.load(Ordering::Relaxed) % self.api_mirrors.len();
        self.api_mirrors[idx].clone()
    }

    /// Ставит зеркало первым, например запомненное с прошлого запуска.
    pub fn prefer_api(&self, base: &str) {
        if let Some(idx) = self.api_mirrors.iter().position(|m| m == base.trim_end_matches('/')) {
            self.active_api.store(idx, Ordering::Relaxed);
        }
    }

    pub fn auth_mirrors(&self) -> &[String] {
        &self.auth_mirrors
    }

    pub async fn set_token(&self, token: Option<Token>) {
        *self.token.write().await = token;
    }

    pub async fn token(&self) -> Option<Token> {
        self.token.read().await.clone()
    }

    pub async fn is_logged_in(&self) -> bool {
        self.token.read().await.is_some()
    }

    pub(crate) fn notify_token(&self, token: Option<&Token>) {
        if let Some(listener) = &self.listener {
            listener(token);
        }
    }

    /// Ждёт своей очереди, чтобы не долбить API чаще заданного интервала.
    async fn throttle(&self) {
        let wait_until = {
            let mut next = self.limiter.lock().await;
            let now = Instant::now();
            let slot = (*next).max(now);
            *next = slot + self.config.api_interval;
            slot
        };
        tokio::time::sleep_until(wait_until).await;
    }

    pub(crate) fn site_headers(site: Site) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Site-Id", HeaderValue::from(u16::from(site.id())));
        let origin = site.origin();
        if let Ok(v) = HeaderValue::from_str(&format!("{origin}/")) {
            headers.insert(REFERER, v);
        }
        if let Ok(v) = HeaderValue::from_str(&origin) {
            headers.insert(ORIGIN, v);
        }
        headers
    }

    /// GET к API с разбором `{"data": T}`.
    pub(crate) async fn get_data<T, Q>(&self, site: Site, path: &str, query: &Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        let body: Data<T> = self.get_json(site, path, query).await?;
        Ok(body.data)
    }

    /// GET к API с разбором всего тела ответа.
    pub(crate) async fn get_json<T, Q>(&self, site: Site, path: &str, query: &Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        let response = self
            .send_api(Auth::User, |http, base| {
                http.get(join_url(base, path))
                    .headers(Self::site_headers(site))
                    .query(query)
            })
            .await?;
        let bytes = response.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: path.to_string(),
            source,
        })
    }

    /// Отправляет запрос к API: очередь, зеркала, токен, повторы, одно обновление
    /// токена при 401. `build` получает адрес текущего зеркала API.
    pub(crate) async fn send_api<F>(&self, auth: Auth, build: F) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let mirrors = self.api_mirrors.len();
        let mut refreshed = false;
        let mut attempt = 0;
        let mut mirrors_tried = 1;
        loop {
            self.throttle().await;
            let idx = self.active_api.load(Ordering::Relaxed) % mirrors;
            let base = &self.api_mirrors[idx];
            let token = match auth {
                Auth::User => self.token.read().await.clone(),
                Auth::None => None,
            };
            let mut request = build(&self.http, base).header(ACCEPT, "application/json");
            if let Some(token) = &token {
                request = request.header(AUTHORIZATION, token.header_value());
            }

            let response = match request.send().await {
                Ok(r) if !is_mirror_block(&r) => r,
                // Зеркало закрыто для этой сети или не отвечает: следующее, без паузы.
                blocked if mirrors_tried < mirrors && mirror_unusable(&blocked) => {
                    self.switch_mirror(idx);
                    mirrors_tried += 1;
                    continue;
                }
                Ok(r) => return Err(status_error(r).await),
                Err(e) if attempt < RETRIES && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    attempt += 1;
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status == StatusCode::UNAUTHORIZED && auth == Auth::User && token.is_some() && !refreshed {
                refreshed = true;
                // Обновление токена само идёт через send_api, поэтому future в коробке.
                Box::pin(self.refresh_after_401(token.as_ref())).await?;
                continue;
            }
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) && attempt < RETRIES {
                attempt += 1;
                let delay = retry_after(&response).unwrap_or_else(|| backoff_delay(attempt));
                tokio::time::sleep(delay).await;
                continue;
            }
            return Err(status_error(response).await);
        }
    }

    /// Переключает на следующее зеркало, если соседний запрос ещё не переключил.
    fn switch_mirror(&self, from: usize) {
        let next = (from + 1) % self.api_mirrors.len();
        let _ = self
            .active_api
            .compare_exchange(from, next, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Обновляет токен один раз на всех, даже если 401 словили несколько запросов сразу.
    async fn refresh_after_401(&self, stale: Option<&Token>) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.token.read().await.clone();
        // Пока ждали замок, токен мог обновить соседний запрос.
        if current.as_ref().map(|t| &t.access_token) != stale.map(|t| &t.access_token) {
            return Ok(());
        }
        match current {
            Some(token) if token.refresh_token.is_some() => match self.refresh_token(&token).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    self.set_token(None).await;
                    self.notify_token(None);
                    Err(e)
                }
            },
            _ => {
                self.set_token(None).await;
                self.notify_token(None);
                Err(Error::Unauthorized)
            }
        }
    }

    /// Упорядочивает адреса одного файла на разных серверах: недавно отказавшие в конец.
    pub fn order_by_health(&self, mut urls: Vec<String>) -> Vec<String> {
        let bad = self.bad_hosts.lock().expect("bad_hosts отравлен");
        urls.sort_by_key(|u| bad.contains(mirrors::host_of(u)));
        urls
    }

    pub(crate) fn mark_host(&self, url: &str, ok: bool) {
        let host = mirrors::host_of(url).to_string();
        let mut bad = self.bad_hosts.lock().expect("bad_hosts отравлен");
        if ok {
            bad.remove(&host);
        } else {
            bad.insert(host);
        }
    }

    /// Загружает файл (картинку, субтитры) с сервера контента. Без очереди API,
    /// параллельность ограничивает вызывающий код.
    pub async fn fetch_bytes(&self, site: Site, url: &str) -> Result<bytes::Bytes> {
        let mut attempt = 0;
        loop {
            let result = self
                .http
                .get(url)
                .header(REFERER, format!("{}/", site.origin()))
                .send()
                .await;
            match result {
                Ok(r) if r.status().is_success() => return Ok(r.bytes().await?),
                Ok(r)
                    if (r.status() == StatusCode::TOO_MANY_REQUESTS || r.status().is_server_error())
                        && attempt < RETRIES => {}
                Ok(r) => return Err(status_error(r).await),
                // Обрыв на середине тела или таймаут повторяем, отказ в соединении нет:
                // у файла есть другие серверы.
                Err(e) if attempt < RETRIES && (e.is_timeout() || e.is_body()) => {}
                Err(e) => return Err(e.into()),
            }
            attempt += 1;
            backoff(attempt).await;
        }
    }

    /// Проверяет, что файл отдаётся, не скачивая его.
    pub async fn probe_url(&self, site: Site, url: &str) -> bool {
        let ok = self
            .http
            .get(url)
            .header(REFERER, format!("{}/", site.origin()))
            .header(reqwest::header::RANGE, "bytes=0-0")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        self.mark_host(url, ok);
        ok
    }

    /// Начинает потоковую загрузку большого файла (видео). Вызывающий читает тело сам.
    pub async fn fetch_stream(&self, site: Site, url: &str, resume_from: u64) -> Result<Response> {
        let mut request = self
            .http
            .get(url)
            .header(REFERER, format!("{}/", site.origin()))
            .timeout(Duration::from_secs(60 * 60 * 6));
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
        }
        let response = request.send().await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(status_error(response).await)
        }
    }
}

/// Ответ похож на страницу блокировки (DDoS-Guard, гео-фильтр), а не на ответ API.
fn is_mirror_block(response: &Response) -> bool {
    let html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    match response.status() {
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => html,
        // Cloudflare и DDoS-Guard отдают свои коды 52x, когда зеркало лежит.
        s => (520..=530).contains(&s.as_u16()),
    }
}

/// Зеркало вернуло страницу блокировки либо до него не достучаться.
/// Сюда попадают только ответы, уже признанные блокировкой.
fn mirror_unusable(result: &std::result::Result<Response, reqwest::Error>) -> bool {
    match result {
        Ok(_) => true,
        Err(e) => e.is_connect() || e.is_timeout(),
    }
}

pub(crate) fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(5))
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(backoff_delay(attempt)).await;
}

fn retry_after(response: &Response) -> Option<Duration> {
    let secs: u64 = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(60)))
}

pub(crate) async fn status_error(response: Response) -> Error {
    match response.status() {
        StatusCode::UNAUTHORIZED => Error::Unauthorized,
        StatusCode::FORBIDDEN => Error::Forbidden,
        StatusCode::NOT_FOUND => Error::NotFound,
        status => {
            let mut body = response.text().await.unwrap_or_default();
            if let Some((cut, _)) = body.char_indices().nth(300) {
                body.truncate(cut);
            }
            Error::Http { status, body }
        }
    }
}
