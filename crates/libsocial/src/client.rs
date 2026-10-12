use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, ORIGIN, REFERER};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::auth::Token;
use crate::error::{Error, Result};
use crate::models::{Constants, Data};
use crate::site::Site;

pub const DEFAULT_API_BASE: &str = "https://api.cdnlibs.org/api";
pub const DEFAULT_AUTH_BASE: &str = "https://auth.lib.social";

/// Сколько раз повторять запрос при 429, 5xx и обрыве сети.
const RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub api_base: String,
    pub auth_base: String,
    pub user_agent: String,
    /// Сетевой интерфейс для всех запросов (только Linux). Нужен, когда системный
    /// VPN выводит трафик в страну, из которой сервис недоступен.
    pub interface: Option<String>,
    /// Прокси вида `socks5h://127.0.0.1:1080` или `http://...`.
    pub proxy: Option<String>,
    /// Пауза между запросами к API.
    pub api_interval: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_base: DEFAULT_API_BASE.into(),
            auth_base: DEFAULT_AUTH_BASE.into(),
            user_agent: format!("Polka/{} (unofficial lib.social client)", env!("CARGO_PKG_VERSION")),
            interface: None,
            proxy: None,
            api_interval: Duration::from_millis(500),
        }
    }
}

/// Вызывается, когда клиент получил или обновил токен, чтобы приложение его сохранило.
pub type TokenListener = Arc<dyn Fn(Option<&Token>) + Send + Sync>;

/// Клиент API. Дёшево клонируется, все клоны делят токен, кэш и ограничитель частоты.
#[derive(Clone)]
pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) config: Arc<ClientConfig>,
    limiter: Arc<Mutex<Instant>>,
    pub(crate) token: Arc<RwLock<Option<Token>>>,
    refresh_lock: Arc<Mutex<()>>,
    listener: Option<TokenListener>,
    pub(crate) constants: Arc<RwLock<Option<Constants>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Client> {
        let mut builder = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(Duration::from_secs(20))
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
            config: Arc::new(config),
            limiter: Arc::new(Mutex::new(Instant::now())),
            token: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
            listener: None,
            constants: Arc::new(RwLock::new(None)),
        })
    }

    pub fn with_token_listener(mut self, listener: TokenListener) -> Client {
        self.listener = Some(listener);
        self
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
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

    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.api_base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
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
        let url = self.api_url(path);
        let response = self
            .send_api(|http| {
                http.request(Method::GET, &url)
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

    /// Отправляет запрос к API: очередь, токен, повторы, одно обновление токена при 401.
    pub(crate) async fn send_api<F>(&self, build: F) -> Result<Response>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            self.throttle().await;
            let token = self.token.read().await.clone();
            let mut request = build(&self.http).header(ACCEPT, "application/json");
            if let Some(token) = &token {
                request = request.header(AUTHORIZATION, token.header_value());
            }
            let result = request.send().await;
            let response = match result {
                Ok(r) => r,
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
            if status == StatusCode::UNAUTHORIZED && token.is_some() && !refreshed {
                refreshed = true;
                self.refresh_after_401(token.as_ref()).await?;
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
                Err(e) if attempt < RETRIES && (e.is_timeout() || e.is_connect() || e.is_body()) => {}
                Err(e) => return Err(e.into()),
            }
            attempt += 1;
            backoff(attempt).await;
        }
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
            if body.len() > 300 {
                let cut = body.char_indices().nth(300).map(|(i, _)| i).unwrap_or(body.len());
                body.truncate(cut);
            }
            Error::Http { status, body }
        }
    }
}
