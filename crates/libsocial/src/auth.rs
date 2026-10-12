//! Вход через OAuth2 с PKCE тем же способом, что и сайт.
//!
//! Пользователь открывает адрес авторизации в браузере, входит, сайт перекидывает его на
//! свою страницу callback. Адрес этой страницы пользователь вставляет в приложение,
//! из него берётся код и меняется на токены.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::Me;
use crate::site::Site;

/// Публичный веб-клиент lib.social. Свой клиент для десктопа выдают разработчики сервиса.
pub const WEB_CLIENT_ID: &str = "1";
pub const WEB_REDIRECT_URI: &str = "https://mangalib.me/ru/front/auth/oauth/callback";

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

impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("state", &self.state)
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

/// Одна попытка входа: секреты PKCE и адрес, который надо открыть в браузере.
#[derive(Clone)]
pub struct AuthSession {
    verifier: String,
    pub state: String,
    pub authorize_url: String,
    client_id: String,
    redirect_uri: String,
}

impl AuthSession {
    pub fn new(auth_base: &str) -> AuthSession {
        Self::with_client(auth_base, WEB_CLIENT_ID, WEB_REDIRECT_URI)
    }

    pub fn with_client(auth_base: &str, client_id: &str, redirect_uri: &str) -> AuthSession {
        let verifier = random_token(96);
        let state = random_token(30);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse(auth_base).expect("auth_base должен быть корректным адресом");
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
        AuthSession {
            verifier,
            state,
            authorize_url: url.into(),
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
        }
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
        let url = format!("{}/auth/oauth/token", self.config.api_base.trim_end_matches('/'));
        // Запрос без Authorization: старый токен тут не нужен и может мешать.
        let response = self
            .http
            .request(Method::POST, &url)
            .headers(Client::site_headers(Site::MangaLib))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Auth(format!(
                "сервер отказал в токене ({status}): {}",
                text.chars().take(200).collect::<String>()
            )));
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

    #[test]
    fn authorize_url_has_pkce() {
        let s = AuthSession::new("https://auth.lib.social");
        let url = Url::parse(&s.authorize_url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("auth.lib.social"));
        assert_eq!(q["client_id"], "1");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], WEB_REDIRECT_URI);
        assert_eq!(q["state"], s.state);
        assert!(s.verifier.len() >= 43 && s.verifier.len() <= 128);
    }

    #[test]
    fn extract_code_checks_state() {
        let s = AuthSession::new("https://auth.lib.social");
        let good = format!("{WEB_REDIRECT_URI}?code=abc&state={}", s.state);
        assert_eq!(s.extract_code(&good).unwrap(), "abc");
        let bad = format!("{WEB_REDIRECT_URI}?code=abc&state=other");
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
