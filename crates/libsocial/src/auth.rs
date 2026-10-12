//! Вход через OAuth2 с PKCE тем же способом, что и сайт.
//!
//! Пользователь открывает адрес авторизации в браузере, входит, сайт перекидывает его на
//! свою страницу callback. Адрес этой страницы пользователь вставляет в приложение,
//! из него берётся код и меняется на токены.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::client::{Auth, Client, join_url, status_error};
use crate::error::{Error, Result};
use crate::mirrors;
use crate::models::Me;
use crate::site::Site;

/// Публичный веб-клиент lib.social. Свой клиент для десктопа выдают разработчики сервиса.
pub const WEB_CLIENT_ID: &str = "1";
/// Путь страницы возврата после входа. Веб-клиент принимает его на любом домене сети.
pub const CALLBACK_PATH: &str = "/ru/front/auth/oauth/callback";

/// Сколько секунд до истечения считать токен уже просроченным.
const EXPIRY_MARGIN: u64 = 120;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Unix-время истечения access token, секунды.
    pub expires_at: u64,
}

fn default_token_type() -> String {
    "Bearer".into()
}

impl Token {
    pub fn header_value(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }

    pub fn is_expired(&self) -> bool {
        now_secs() + EXPIRY_MARGIN >= self.expires_at
    }
}

/// Токены не попадают в логи.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    token_type: String,
    #[serde(default)]
    expires_in: u64,
}

impl From<TokenResponse> for Token {
    fn from(r: TokenResponse) -> Token {
        let lifetime = if r.expires_in == 0 { 3600 } else { r.expires_in };
        Token {
            access_token: r.access_token,
            refresh_token: r.refresh_token,
            token_type: r.token_type,
            expires_at: now_secs() + lifetime,
        }
    }
}

/// Одна попытка входа: секреты PKCE и адреса, которые можно открыть в браузере.
#[derive(Clone)]
pub struct AuthSession {
    verifier: String,
    pub state: String,
    /// Адрес входа на каждом сервере авторизации. Все с одним PKCE, годится любой.
    pub authorize_urls: Vec<String>,
    client_id: String,
    redirect_uri: String,
}

impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl AuthSession {
    /// Вход веб-клиентом с возвратом на `web_origin` (например `https://mangalib.org`).
    pub fn new(auth_mirrors: &[String], web_origin: &str) -> AuthSession {
        let redirect = format!("{}{CALLBACK_PATH}", web_origin.trim_end_matches('/'));
        Self::with_client(auth_mirrors, WEB_CLIENT_ID, &redirect)
    }

    pub fn with_client(auth_mirrors: &[String], client_id: &str, redirect_uri: &str) -> AuthSession {
        let verifier = random_token(96);
        let state = random_token(30);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let authorize_urls = auth_mirrors
            .iter()
            .filter_map(|base| Url::parse(base).ok())
            .map(|mut url| {
                url.set_path("/auth/oauth/authorize");
                url.query_pairs_mut()
                    .append_pair("scope", "")
                    .append_pair("client_id", client_id)
                    .append_pair("response_type", "code")
                    .append_pair("redirect_uri", redirect_uri)
                    .append_pair("state", &state)
                    .append_pair("code_challenge", &challenge)
                    .append_pair("code_challenge_method", "S256")
                    .append_pair("prompt", "consent");
                url.into()
            })
            .collect();
        AuthSession {
            verifier,
            state,
            authorize_urls,
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
        }
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Достаёт код из вставленного адреса callback и проверяет `state`.
    /// Принимает и голый код, если пользователь скопировал только его.
    pub fn extract_code(&self, pasted: &str) -> Result<String> {
        let pasted = pasted.trim();
        if pasted.is_empty() {
            return Err(Error::Auth(
                "вставьте адрес страницы, на которую попали после входа".into(),
            ));
        }
        let Ok(url) = Url::parse(pasted) else {
            return Ok(pasted.to_string());
        };
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => return Err(Error::Auth(format!("сайт вернул ошибку: {value}"))),
                _ => {}
            }
        }
        if let Some(state) = state
            && state != self.state
        {
            return Err(Error::Auth("адрес от другой попытки входа, начните вход заново".into()));
        }
        code.ok_or_else(|| Error::Auth("в адресе нет кода авторизации".into()))
    }
}

impl Client {
    /// Новая попытка входа веб-клиентом через зеркала из настроек клиента.
    pub fn begin_login(&self) -> AuthSession {
        AuthSession::new(self.auth_mirrors(), mirrors::web(Site::MangaLib)[0])
    }

    /// Меняет код из callback на токены и запоминает их.
    pub async fn exchange_code(&self, session: &AuthSession, code: &str) -> Result<Token> {
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": session.client_id,
            "redirect_uri": session.redirect_uri,
            "code_verifier": session.verifier,
            "code": code,
        });
        let token = self.token_request(&body).await?;
        self.set_token(Some(token.clone())).await;
        self.notify_token(Some(&token));
        Ok(token)
    }

    /// Обновляет access token по refresh token.
    pub async fn refresh_token(&self, current: &Token) -> Result<Token> {
        let refresh = current.refresh_token.as_deref().ok_or(Error::Unauthorized)?;
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": WEB_CLIENT_ID,
            "refresh_token": refresh,
            "scope": "",
        });
        let mut token = self.token_request(&body).await?;
        if token.refresh_token.is_none() {
            token.refresh_token = current.refresh_token.clone();
        }
        self.set_token(Some(token.clone())).await;
        self.notify_token(Some(&token));
        Ok(token)
    }

    /// Поднимает сохранённый токен при запуске: обновляет его, если истёк.
    pub async fn restore_token(&self, token: Token) -> Result<Token> {
        if token.is_expired() {
            return self.refresh_token(&token).await;
        }
        self.set_token(Some(token.clone())).await;
        Ok(token)
    }

    pub async fn logout(&self) {
        self.set_token(None).await;
        self.notify_token(None);
    }

    pub async fn me(&self) -> Result<Me> {
        self.get_data(Site::MangaLib, "/auth/me", &()).await
    }

    async fn token_request(&self, body: &serde_json::Value) -> Result<Token> {
        // Без Authorization: старый токен тут не нужен и может мешать.
        let response = match self
            .send_api(Auth::None, |http, base| {
                http.post(join_url(base, "auth/oauth/token"))
                    .headers(Client::site_headers(Site::MangaLib))
                    .json(body)
            })
            .await
        {
            Ok(r) => r,
            Err(Error::Http { status, body }) => {
                return Err(Error::Auth(format!("сервер отказал в токене ({status}): {body}")));
            }
            Err(Error::Unauthorized) => return Err(Error::Auth("сервер отказал в токене".into())),
            Err(e) => return Err(e),
        };
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let bytes = response.bytes().await?;
        let parsed: TokenResponse = serde_json::from_slice(&bytes)
            .or_else(|_| serde_json::from_slice::<crate::models::Data<TokenResponse>>(&bytes).map(|d| d.data))
            .map_err(|source| Error::Decode {
                context: "токен".into(),
                source,
            })?;
        Ok(parsed.into())
    }
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(buf.as_mut_slice());
    URL_SAFE_NO_PAD.encode(buf)
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> AuthSession {
        let mirrors = mirrors::or_default(&[], mirrors::AUTH);
        AuthSession::new(&mirrors, "https://mangalib.org/")
    }

    #[test]
    fn authorize_urls_have_pkce() {
        let s = session();
        assert_eq!(s.authorize_urls.len(), mirrors::AUTH.len());
        let url = Url::parse(&s.authorize_urls[0]).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("auth.lib.social"));
        assert_eq!(q["client_id"], "1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], "https://mangalib.org/ru/front/auth/oauth/callback");
        assert_eq!(q["state"], s.state);
        assert!(s.verifier.len() >= 43 && s.verifier.len() <= 128);
        assert!(
            s.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        let second = Url::parse(&s.authorize_urls[1]).unwrap();
        assert_eq!(second.host_str(), Some("auth.cdnlibs.org"));
    }

    #[test]
    fn extract_code_checks_state() {
        let s = session();
        let good = format!("{}?code=abc&state={}", s.redirect_uri(), s.state);
        assert_eq!(s.extract_code(&good).unwrap(), "abc");
        let bad = format!("{}?code=abc&state=other", s.redirect_uri());
        assert!(s.extract_code(&bad).is_err());
        assert_eq!(s.extract_code("  rawcode ").unwrap(), "rawcode");
    }

    #[test]
    fn expiry() {
        let t = Token {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_at: now_secs() + 10,
        };
        assert!(t.is_expired());
        let t = Token {
            expires_at: now_secs() + 3600,
            ..t
        };
        assert!(!t.is_expired());
    }
}
