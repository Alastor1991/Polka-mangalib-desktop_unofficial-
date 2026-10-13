//! Логика клиента без сети: зеркала, повторы, обновление токена. Сервер – wiremock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use libsocial::auth::now_secs as now;
use libsocial::{Client, ClientConfig, Error, Site, Token};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(mirrors: &[&MockServer]) -> Client {
    let config = ClientConfig {
        api_mirrors: mirrors.iter().map(|m| format!("{}/api", m.uri())).collect(),
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

fn token(access: &str) -> Token {
    Token {
        access_token: access.into(),
        refresh_token: Some("refresh-1".into()),
        token_type: "Bearer".into(),
        expires_at: now() + 3600,
        client_id: "1".into(),
    }
}

fn me_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 7, "username": "tester"}}))
}

fn html(status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw("<html>blocked</html>", "text/html; charset=utf-8")
}

/// Слушатель токенов, который запоминает всё, что ему прислали.
fn recorder(client: Client) -> (Client, Arc<Mutex<Vec<Option<String>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let client = client.with_token_listener(Arc::new(move |t: Option<&Token>| {
        sink.lock().unwrap().push(t.map(|t| t.access_token.clone()));
    }));
    (client, seen)
}

#[tokio::test]
async fn switches_mirror_on_block_page() {
    let blocked = MockServer::start().await;
    let good = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(html(403))
        .mount(&blocked)
        .await;
    Mock::given(path("/api/auth/me"))
        .respond_with(me_ok())
        .mount(&good)
        .await;

    let c = client(&[&blocked, &good]);
    assert_eq!(c.me().await.unwrap().username, "tester");
    assert_eq!(c.api_base(), format!("{}/api", good.uri()));
}

#[tokio::test]
async fn json_403_is_not_a_mirror_problem() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "no"})))
        .mount(&first)
        .await;
    Mock::given(path("/api/auth/me"))
        .respond_with(me_ok())
        .expect(0)
        .mount(&second)
        .await;

    let c = client(&[&first, &second]);
    assert!(matches!(c.me().await, Err(Error::Forbidden)));
    assert_eq!(c.api_base(), format!("{}/api", first.uri()));
}

#[tokio::test]
async fn all_mirrors_blocked_keeps_original() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(html(403))
        .mount(&a)
        .await;
    Mock::given(path("/api/auth/me"))
        .respond_with(html(403))
        .mount(&b)
        .await;

    let c = client(&[&a, &b]);
    assert!(matches!(c.me().await, Err(Error::Forbidden)));
    assert_eq!(c.api_base(), format!("{}/api", a.uri()));
}

#[tokio::test]
async fn gateway_down_everywhere_is_service_down() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    // Retry-After: 0, чтобы повторы не ждали 2, 4 и 8 секунд.
    let bad_gateway = || {
        ResponseTemplate::new(502)
            .insert_header("Retry-After", "0")
            .set_body_raw("<html>502 Bad Gateway</html>", "text/html")
    };
    // Первый запрос и три повтора на исходном зеркале, один запрос на втором.
    Mock::given(path("/api/auth/me"))
        .respond_with(bad_gateway())
        .expect(4)
        .mount(&a)
        .await;
    Mock::given(path("/api/auth/me"))
        .respond_with(bad_gateway())
        .expect(1)
        .mount(&b)
        .await;

    let c = client(&[&a, &b]);
    let err = c.me().await.unwrap_err();
    assert!(matches!(err, Error::ServiceDown { .. }), "{err:?}");
    assert_eq!(c.api_base(), format!("{}/api", a.uri()));
}

#[tokio::test]
async fn gateway_recovers_after_retry() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(path("/api/auth/me")).respond_with(me_ok()).mount(&s).await;

    let c = client(&[&s]);
    assert_eq!(c.me().await.unwrap().id, 7);
}

#[tokio::test]
async fn long_retry_after_stops_api_calls() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .expect(1)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    match c.me().await {
        Err(Error::RateLimited { retry_after }) => assert_eq!(retry_after, Duration::from_secs(600)),
        other => panic!("ждали RateLimited, пришло {other:?}"),
    }
    // До конца паузы клиент к серверу не ходит: expect(1) это проверит.
    assert!(matches!(c.me().await, Err(Error::RateLimited { .. })));
    let left = c.cooldown_remaining().unwrap();
    assert!(left > Duration::from_secs(590) && left <= Duration::from_secs(600));
}

#[tokio::test]
async fn long_retry_after_on_image_server() {
    let s = MockServer::start().await;
    Mock::given(path("/x.jpg"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "300"))
        .expect(1)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    let err = c.fetch_bytes(Site::MangaLib, &format!("{}/x.jpg", s.uri())).await;
    assert!(matches!(err, Err(Error::RateLimited { .. })), "{err:?}");
}

#[tokio::test]
async fn retries_429() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(path("/api/auth/me")).respond_with(me_ok()).mount(&s).await;

    let c = client(&[&s]);
    assert_eq!(c.me().await.unwrap().id, 7);
}

#[tokio::test]
async fn refreshes_token_on_401() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .and(header("Authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "expired"})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_type": "Bearer", "expires_in": 3600,
            "access_token": "new", "refresh_token": "refresh-2"
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/api/auth/me"))
        .and(header("Authorization", "Bearer new"))
        .respond_with(me_ok())
        .mount(&s)
        .await;

    let (c, seen) = recorder(client(&[&s]));
    c.set_token(Some(token("old"))).await;
    assert_eq!(c.me().await.unwrap().username, "tester");
    let t = c.token().await.unwrap();
    assert_eq!(t.access_token, "new");
    assert_eq!(t.refresh_token.as_deref(), Some("refresh-2"));
    assert_eq!(*seen.lock().unwrap(), vec![Some("new".to_string())]);
}

#[tokio::test]
async fn rejected_refresh_drops_token() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&s)
        .await;
    Mock::given(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&s)
        .await;

    let (c, seen) = recorder(client(&[&s]));
    c.set_token(Some(token("old"))).await;
    assert!(matches!(c.me().await, Err(Error::Unauthorized)));
    assert!(c.token().await.is_none());
    assert_eq!(*seen.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn failed_refresh_keeps_token() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&s)
        .await;
    Mock::given(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&s)
        .await;

    let (c, seen) = recorder(client(&[&s]));
    c.set_token(Some(token("old"))).await;
    assert!(c.me().await.is_err());
    assert_eq!(c.token().await.unwrap().access_token, "old");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn expired_token_restored_offline() {
    let mut t = token("old");
    t.expires_at = now() - 10;
    // Порт 9 закрыт: до сервера не достучаться, но токен должен остаться.
    let config = ClientConfig {
        api_mirrors: vec!["http://127.0.0.1:9/api".into()],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    let c = Client::new(config).unwrap();
    assert!(c.restore_token(t).await.is_err());
    assert_eq!(c.token().await.unwrap().access_token, "old");
}

#[tokio::test]
async fn page_falls_back_to_other_server() {
    use libsocial::models::PageImage;

    let api = MockServer::start().await;
    let dead = MockServer::start().await;
    let alive = MockServer::start().await;
    Mock::given(path("/api/constants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "imageServers": [
                {"id": "compress", "label": "", "url": dead.uri(), "site_ids": [1]},
                {"id": "main", "label": "", "url": alive.uri(), "site_ids": [1]}
            ],
            "videoServers": []
        }})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(path("/manga/x/1.jpg"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&dead)
        .await;
    Mock::given(path("/manga/x/1.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes([vec![0xFF, 0xD8, 0xFF, 0xE0], vec![7u8; 4092]].concat()),
        )
        .mount(&alive)
        .await;

    let c = client(&[&api]);
    let page: PageImage = serde_json::from_value(json!({"slug": 1, "url": "//manga/x/1.jpg"})).unwrap();
    let bytes = c.fetch_page(Site::MangaLib, &page, "compress").await.unwrap();
    assert_eq!(bytes.len(), 4096);
    // Второй раз упавший сервер идёт последним, а справочник берётся из кэша.
    let bytes = c.fetch_page(Site::MangaLib, &page, "compress").await.unwrap();
    assert_eq!(bytes.len(), 4096);
}

#[tokio::test]
async fn own_oauth_client_used_for_login_and_refresh() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "authorization_code", "client_id": "7"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "expires_in": 3600, "access_token": "a1", "refresh_token": "r1"
        })))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "refresh_token", "client_id": "7"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "expires_in": 3600, "access_token": "a2", "refresh_token": "r2"
        })))
        .expect(1)
        .mount(&s)
        .await;

    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", s.uri())],
        api_interval: Duration::ZERO,
        oauth_client_id: Some("7".into()),
        oauth_redirect_uri: Some("http://127.0.0.1:8765/callback".into()),
        ..Default::default()
    };
    let c = Client::new(config).unwrap();
    let session = c.begin_login();
    assert!(session.authorize_urls[0].contains("client_id=7"));
    assert_eq!(session.redirect_uri(), "http://127.0.0.1:8765/callback");

    let token = c.exchange_code(&session, "code").await.unwrap();
    assert_eq!(token.client_id, "7");
    let fresh = c.refresh_token(&token).await.unwrap();
    assert_eq!(fresh.access_token, "a2");
    assert_eq!(fresh.client_id, "7");
}

#[tokio::test]
async fn page_reports_rate_limit_over_other_errors() {
    use libsocial::models::PageImage;

    let api = MockServer::start().await;
    let limited = MockServer::start().await;
    let missing = MockServer::start().await;
    Mock::given(path("/api/constants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "imageServers": [
                {"id": "compress", "label": "", "url": limited.uri(), "site_ids": [1]},
                {"id": "main", "label": "", "url": missing.uri(), "site_ids": [1]}
            ],
            "videoServers": []
        }})))
        .mount(&api)
        .await;
    // Сервер, который попросил паузу, во втором круге не трогаем.
    Mock::given(path("/manga/x/1.jpg"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .expect(1)
        .mount(&limited)
        .await;
    Mock::given(path("/manga/x/1.jpg"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&missing)
        .await;

    let c = client(&[&api]);
    let page: PageImage = serde_json::from_value(json!({"slug": 1, "url": "//manga/x/1.jpg"})).unwrap();
    let err = c.fetch_page(Site::MangaLib, &page, "compress").await;
    assert!(matches!(err, Err(Error::RateLimited { .. })), "{err:?}");
}
