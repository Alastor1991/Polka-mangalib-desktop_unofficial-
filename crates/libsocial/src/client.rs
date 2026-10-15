use std::collections::HashMap;
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
/// Сколько помнить, что сервер картинок или видео не отвечал.
const BAD_HOST_TTL: Duration = Duration::from_secs(5 * 60);
/// Сколько не ходить за справочником после неудачи.
const CONSTANTS_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Дольше этого запрос внутри себя не ждёт. Если сервис просит паузу длиннее,
/// клиент сразу отвечает `Error::RateLimited` и не ходит к API до её конца.
const MAX_INLINE_WAIT: Duration = Duration::from_secs(60);
/// Верхняя граница `Retry-After` на случай мусора в заголовке.
const MAX_COOLDOWN: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
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
    /// OAuth-клиент для входа. Без него используется веб-клиент сайта.
    pub oauth_client_id: Option<String>,
    /// Адрес возврата после входа. Без него страница callback сайта MangaLib.
    pub oauth_redirect_uri: Option<String>,
}

/// Прокси может содержать логин и пароль, в логи он не попадает.
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("api_mirrors", &self.api_mirrors)
            .field("auth_mirrors", &self.auth_mirrors)
            .field("interface", &self.interface)
            .field("proxy", &self.proxy.as_ref().map(|_| "<задан>"))
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_redirect_uri", &self.oauth_redirect_uri)
            .finish_non_exhaustive()
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_mirrors: Vec::new(),
            auth_mirrors: Vec::new(),
            user_agent: format!(
                "Polka/{} (unofficial lib.social client; +{})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_REPOSITORY")
            ),
            interface: None,
            proxy: None,
            api_interval: Duration::from_millis(500),
            oauth_client_id: None,
            oauth_redirect_uri: None,
        }
    }
}

/// Вызывается, когда клиент получил, обновил или окончательно потерял токен,
/// чтобы приложение его сохранило или стёрло.
pub type TokenListener = Arc<dyn Fn(Option<&Token>) + Send + Sync>;

#[derive(Default)]
struct ConstantsCache {
    value: Option<Constants>,
    failed_at: Option<Instant>,
}

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
    pub(crate) refresh_lock: Arc<Mutex<()>>,
    listener: Option<TokenListener>,
    constants: Arc<Mutex<ConstantsCache>>,
    /// Хосты картинок и видео, которые недавно не ответили. Их пробуем последними.
    bad_hosts: Arc<std::sync::Mutex<HashMap<String, Instant>>>,
    /// До этого момента сервис просил не присылать запросы к API.
    cooldown: Arc<std::sync::Mutex<Option<Instant>>>,
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
    /// Без токена и без повторов после отправки: обмен кода и обновление токена.
    /// Эти запросы одноразовые, повтор после потерянного ответа сжёг бы код.
    TokenEndpoint,
    /// Отзыв токена при выходе. Токен к этому времени уже сброшен, его кладёт в
    /// заголовок сам запрос. Без обновления при 401 (протухший отзывать незачем)
    /// и без повторов: выход не должен ждать лежащий сервер.
    Revoke,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Client> {
        let mut builder = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60));
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
            constants: Arc::new(Mutex::new(ConstantsCache::default())),
            bad_hosts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            cooldown: Arc::new(std::sync::Mutex::new(None)),
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

    /// Сколько ещё сервис просил не присылать запросы к API. `None`, если паузы нет.
    pub fn cooldown_remaining(&self) -> Option<Duration> {
        let until = (*self.cooldown.lock().expect("cooldown отравлен"))?;
        let left = until.saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left)
    }

    fn start_cooldown(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut cooldown = self.cooldown.lock().expect("cooldown отравлен");
        *cooldown = Some(cooldown.map_or(until, |current| current.max(until)));
    }

    /// Сервер попросил притормозить: сдвигаем очередь для всех запросов сразу.
    async fn penalize(&self, delay: Duration) {
        let mut next = self.limiter.lock().await;
        *next = (*next).max(Instant::now() + delay);
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

    /// Запрос к API с телом в JSON и разбором `{"data": T}`.
    pub(crate) async fn send_data<T, Q, B>(
        &self,
        site: Site,
        method: reqwest::Method,
        path: &str,
        query: &Q,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
        B: Serialize + ?Sized,
    {
        let bytes = self.send_raw(site, method, path, query, body).await?.bytes().await?;
        let body: Data<T> = serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: path.to_string(),
            source,
        })?;
        Ok(body.data)
    }

    /// Запрос к API от имени сайта с ответом целиком: для тел без обёртки `data` и
    /// двоичных.
    pub(crate) async fn send_raw<Q, B>(
        &self,
        site: Site,
        method: reqwest::Method,
        path: &str,
        query: &Q,
        body: Option<&B>,
    ) -> Result<reqwest::Response>
    where
        Q: Serialize + ?Sized,
        B: Serialize + ?Sized,
    {
        self.send_api(Auth::User, |http, base| {
            let request = http
                .request(method.clone(), join_url(base, path))
                .headers(Self::site_headers(site))
                .query(query);
            match body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await
    }

    /// Запрос на запись, ответ которого – одна запись. Есть ли у таких ответов обёртка
    /// `data`, по скриптам сайта часто не видно, поэтому годится и с ней, и без неё.
    /// Запись на сервере к разбору ответа уже сделана: ответ без записи (пустое тело,
    /// `null`, пустой список) даёт `None`, а не ошибку, чтобы вызывающий не повторил
    /// запрос. Состояние тогда перечитывают отдельным GET.
    pub(crate) async fn send_record<T, B>(
        &self,
        site: Site,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let bytes = self.send_raw(site, method, path, &(), body).await?.bytes().await?;
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Ok(None);
        };
        record_of(&value, path)
    }

    /// Запрос к API, ответ которого не нужен: важно только, что он успешен.
    pub(crate) async fn send_ok<B>(
        &self,
        site: Site,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        self.send_api(Auth::User, |http, base| {
            let request = http
                .request(method.clone(), join_url(base, path))
                .headers(Self::site_headers(site));
            match body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await
        .map(drop)
    }

    /// Отправляет запрос к API: очередь, зеркала, токен, повторы, одно обновление
    /// токена при 401. `build` получает адрес текущего зеркала API.
    ///
    /// Каждый проход цикла тратит одну из конечных попыток (смена зеркала, повтор,
    /// обновление токена), поэтому цикл всегда заканчивается.
    pub(crate) async fn send_api<F>(&self, auth: Auth, build: F) -> Result<Response>
    where
        F: Fn(&reqwest::Client, &str) -> RequestBuilder,
    {
        let mirrors = self.api_mirrors.len();
        let start = self.active_api.load(Ordering::Relaxed) % mirrors;
        let mut refreshed = false;
        let mut attempt = 0;
        let mut mirrors_tried = 1;
        loop {
            if let Some(left) = self.cooldown_remaining() {
                return Err(Error::RateLimited { retry_after: left });
            }
            self.throttle().await;
            let idx = self.active_api.load(Ordering::Relaxed) % mirrors;
            let base = &self.api_mirrors[idx];
            let token = match auth {
                Auth::User => self.token.read().await.clone(),
                Auth::TokenEndpoint | Auth::Revoke => None,
            };
            let mut request = build(&self.http, base).header(ACCEPT, "application/json");
            if let Some(token) = &token {
                request = request.header(AUTHORIZATION, token.header_value());
            }

            let outcome = request.send().await;
            let mirror_down = match &outcome {
                Ok(r) => is_mirror_block(r),
                Err(e) => e.is_connect() || (auth == Auth::User && e.is_timeout()),
            };
            if mirror_down && mirrors_tried < mirrors {
                // Зеркало закрыто для этой сети или не отвечает: следующее, без паузы.
                self.switch_mirror(idx);
                mirrors_tried += 1;
                continue;
            }
            let response = match outcome {
                Ok(r) => r,
                Err(e) => {
                    // Для одноразовых запросов повторяем, только если запрос точно не ушёл.
                    let retry = match auth {
                        Auth::User => e.is_timeout() || e.is_connect() || e.is_request(),
                        Auth::TokenEndpoint => e.is_connect(),
                        Auth::Revoke => false,
                    };
                    if retry && attempt < RETRIES {
                        attempt += 1;
                        backoff(attempt).await;
                        continue;
                    }
                    if mirror_down {
                        // Не достучались ни до одного зеркала: вернёмся к исходному,
                        // чтобы не запомнить последнее как рабочее.
                        self.active_api.store(start, Ordering::Relaxed);
                    }
                    return Err(e.into());
                }
            };
            let status = response.status();
            if status.is_success() && !mirror_down {
                return Ok(response);
            }
            if mirror_down {
                // Зеркала кончились: остаёмся на исходном, последнее рабочим не считаем.
                self.active_api.store(start, Ordering::Relaxed);
                if !is_gateway(status) {
                    return Err(Error::Forbidden);
                }
                // Шлюз упал на всех зеркалах сразу, значит, лежит сам сервис. Дальше
                // это обычный 5xx: повторы с паузами, а не мгновенный отказ.
            }
            if status == StatusCode::UNAUTHORIZED && auth == Auth::User && token.is_some() && !refreshed {
                refreshed = true;
                // Обновление токена само идёт через send_api, поэтому future в коробке.
                Box::pin(self.refresh_after_401(token.as_ref())).await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS
                && let Some(delay) = retry_after(&response).filter(|d| *d > MAX_INLINE_WAIT)
            {
                // Ждать столько внутри запроса нельзя: интерфейс висел бы минутами.
                // Отказываем сразу и молчим до конца паузы, как просил сервис.
                self.start_cooldown(delay);
                return Err(Error::RateLimited { retry_after: delay });
            }
            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && auth == Auth::User && attempt < RETRIES {
                attempt += 1;
                let delay = retry_after(&response)
                    .unwrap_or_else(|| backoff_delay(attempt))
                    .min(MAX_INLINE_WAIT);
                if status == StatusCode::TOO_MANY_REQUESTS {
                    self.penalize(delay).await;
                } else {
                    tokio::time::sleep(delay).await;
                }
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
    /// Токен стирается, только когда сервер явно отверг refresh token.
    async fn refresh_after_401(&self, stale: Option<&Token>) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.token.read().await.clone();
        // Пока ждали замок, токен мог обновить соседний запрос.
        if current.as_ref().map(|t| &t.access_token) != stale.map(|t| &t.access_token) {
            return Ok(());
        }
        let Some(token) = current else {
            return Err(Error::Unauthorized);
        };
        if token.refresh_token.is_none() {
            self.drop_token().await;
            return Err(Error::Unauthorized);
        }
        match self.refresh_unlocked(&token).await {
            Ok(_) => Ok(()),
            Err(Error::RefreshRejected) => {
                self.drop_token().await;
                Err(Error::Unauthorized)
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn drop_token(&self) {
        self.set_token(None).await;
        self.notify_token(None);
    }

    /// Упорядочивает адреса одного файла на разных серверах: недавно отказавшие
    /// в конец, повторы убраны с сохранением порядка.
    pub fn order_by_health(&self, urls: Vec<String>) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut urls: Vec<String> = urls.into_iter().filter(|u| seen.insert(u.clone())).collect();
        let mut bad = self.bad_hosts.lock().expect("bad_hosts отравлен");
        bad.retain(|_, since| since.elapsed() < BAD_HOST_TTL);
        urls.sort_by_key(|u| bad.contains_key(mirrors::host_of(u)));
        urls
    }

    pub(crate) fn mark_host(&self, url: &str, ok: bool) {
        let host = mirrors::host_of(url).to_string();
        let mut bad = self.bad_hosts.lock().expect("bad_hosts отравлен");
        if ok {
            bad.remove(&host);
        } else {
            bad.insert(host, Instant::now());
        }
    }

    /// Загружает файл (картинку, субтитры) с сервера контента с повторами.
    /// Без очереди API, параллельность ограничивает вызывающий код.
    pub async fn fetch_bytes(&self, site: Site, url: &str) -> Result<bytes::Bytes> {
        self.fetch_bytes_tries(site, url, RETRIES).await
    }

    /// Одна попытка плюс `retries` повторов при 429, 5xx, таймауте и обрыве тела.
    /// Сервер, который не отвечает или падает, попадает в журнал плохих хостов.
    pub(crate) async fn fetch_bytes_tries(&self, site: Site, url: &str, retries: u32) -> Result<bytes::Bytes> {
        let mut attempt = 0;
        loop {
            let result = self
                .http
                .get(url)
                .header(REFERER, format!("{}/", site.origin()))
                .send()
                .await;
            let mut wait = None;
            let error = match result {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(bytes) => {
                        self.mark_host(url, true);
                        return Ok(bytes);
                    }
                    Err(e) => Error::Network(e),
                },
                Ok(r) => {
                    if r.status() == StatusCode::TOO_MANY_REQUESTS {
                        wait = retry_after(&r);
                        if let Some(delay) = wait.filter(|d| *d > MAX_INLINE_WAIT) {
                            // Сервер жив, просто просит паузу: в журнал плохих не пишем.
                            return Err(Error::RateLimited { retry_after: delay });
                        }
                    }
                    let transient = r.status() == StatusCode::TOO_MANY_REQUESTS || r.status().is_server_error();
                    let error = status_error(r).await;
                    if !transient {
                        // 404 и подобные говорят о файле, а не о сервере.
                        return Err(error);
                    }
                    error
                }
                Err(e) => Error::Network(e),
            };
            if attempt >= retries {
                self.mark_host(url, false);
                return Err(error);
            }
            attempt += 1;
            tokio::time::sleep(wait.unwrap_or_else(|| backoff_delay(attempt))).await;
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
    /// При докачке сервер обязан ответить 206. Если он отдал файл целиком, это
    /// `RangeUnsupported`, а 416 означает `RangeNotSatisfiable`: дозапись с начала
    /// испортила бы файл.
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
        let status = response.status();
        if resume_from > 0 {
            match status {
                StatusCode::PARTIAL_CONTENT => {}
                StatusCode::RANGE_NOT_SATISFIABLE => return Err(Error::RangeNotSatisfiable),
                s if s.is_success() => return Err(Error::RangeUnsupported),
                _ => return Err(status_error(response).await),
            }
        }
        if status.is_success() {
            Ok(response)
        } else {
            Err(status_error(response).await)
        }
    }

    /// Справочник сервера, кэшируется на время жизни клиента. Параллельные вызовы
    /// ждут одну загрузку, после неудачи полминуты сразу возвращают ошибку.
    pub async fn constants(&self) -> Result<Constants> {
        let mut cache = self.constants.lock().await;
        if let Some(c) = &cache.value {
            return Ok(c.clone());
        }
        if cache.failed_at.is_some_and(|t| t.elapsed() < CONSTANTS_RETRY_AFTER) {
            return Err(Error::Other("справочник сервера недоступен, повторю чуть позже".into()));
        }
        let query = [("fields[]", "imageServers"), ("fields[]", "videoServers")];
        match self.get_data::<Constants, _>(Site::MangaLib, "constants", &query).await {
            Ok(c) => {
                cache.value = Some(c.clone());
                cache.failed_at = None;
                Ok(c)
            }
            Err(e) => {
                cache.failed_at = Some(Instant::now());
                Err(e)
            }
        }
    }
}

/// Ответ похож на страницу блокировки или упавший шлюз, а не на ответ API.
/// Настоящие ответы API, в том числе ошибки, приходят в JSON.
fn is_mirror_block(response: &Response) -> bool {
    let html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    let status = response.status();
    match status.as_u16() {
        200 | 403 | 404 => html,
        _ => is_gateway(status),
    }
}

/// Запись из ответа с обёрткой `data` или без неё. `None`, если записи нет.
pub(crate) fn record_of<T: DeserializeOwned>(value: &serde_json::Value, path: &str) -> Result<Option<T>> {
    let record = value.get("data").unwrap_or(value);
    if !record.is_object() {
        return Ok(None);
    }
    T::deserialize(record).map(Some).map_err(|source| Error::Decode {
        context: path.to_string(),
        source,
    })
}

/// Шлюз перед API лежит или не достучался до сервиса. 52x отдают Cloudflare и DDoS-Guard.
fn is_gateway(status: StatusCode) -> bool {
    matches!(status.as_u16(), 502..=504 | 520..=530)
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
    Some(Duration::from_secs(secs).min(MAX_COOLDOWN))
}

pub(crate) async fn status_error(response: Response) -> Error {
    match response.status() {
        StatusCode::UNAUTHORIZED => Error::Unauthorized,
        StatusCode::FORBIDDEN => Error::Forbidden,
        StatusCode::NOT_FOUND => Error::NotFound,
        // Тело упавшего шлюза – HTML-страница, пользователю в ней читать нечего.
        status if is_gateway(status) => Error::ServiceDown { status },
        status => {
            let mut body = response.text().await.unwrap_or_default();
            if let Some((cut, _)) = body.char_indices().nth(300) {
                body.truncate(cut);
            }
            Error::Http { status, body }
        }
    }
}

impl Client {
    /// Сырой GET к API для отладки и снятия фикстур.
    pub async fn raw_get(&self, site: Site, path: &str, query: &[(String, String)]) -> Result<serde_json::Value> {
        self.get_json(site, path, query).await
    }
}
