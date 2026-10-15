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
async fn logout_revokes_token() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/auth/oauth/token"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;

    let (c, seen) = recorder(client(&[&s]));
    c.set_token(Some(token("old"))).await;
    assert!(c.logout().await);
    assert!(c.token().await.is_none());
    assert_eq!(*seen.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn logout_forgets_token_when_server_refuses() {
    let s = MockServer::start().await;
    // Один запрос: ни повторов на 500, ни обновления токена на 401.
    Mock::given(method("DELETE"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;

    let (c, seen) = recorder(client(&[&s]));
    c.set_token(Some(token("old"))).await;
    assert!(!c.logout().await);
    assert!(c.token().await.is_none());
    assert_eq!(*seen.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn logout_with_stale_token_does_not_refresh() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    c.set_token(Some(token("old"))).await;
    assert!(!c.logout().await);
    assert!(c.token().await.is_none());
}

#[tokio::test]
async fn logout_offline_forgets_token() {
    let config = ClientConfig {
        api_mirrors: vec!["http://127.0.0.1:9/api".into()],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    let (c, seen) = recorder(Client::new(config).unwrap());
    c.set_token(Some(token("old"))).await;
    let started = std::time::Instant::now();
    assert!(!c.logout().await);
    // Без сети выход не ждёт повторов с паузами.
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(c.token().await.is_none());
    assert_eq!(*seen.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn logout_forgets_token_before_server_answers() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .expect(1)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    c.set_token(Some(token("old"))).await;
    let pending = tokio::spawn({
        let c = c.clone();
        async move { c.logout().await }
    });
    // Окно могут закрыть, не дождавшись ответа: токен к этому времени уже забыт.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(c.token().await.is_none());
    pending.abort();
}

#[tokio::test]
async fn logout_without_login_sends_nothing() {
    let s = MockServer::start().await;
    Mock::given(path("/api/auth/oauth/token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    assert!(!c.logout().await);
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

#[tokio::test]
async fn bookmarks_page_and_writes() {
    use wiremock::matchers::query_param;

    let s = MockServer::start().await;
    let fixture =
        std::fs::read_to_string(format!("{}/tests/fixtures/bookmarks.json", env!("CARGO_MANIFEST_DIR"))).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/bookmarks"))
        .and(query_param("user_id", "7"))
        .and(query_param("status", "1"))
        .and(header("Site-Id", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "application/json"))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/bookmarks"))
        .and(body_partial_json(json!({
            "media_type": "anime",
            "media_slug": "1--x",
            "bookmark": {"status": 21, "item_id": 5, "progress": "12:34"},
            "meta": {"item_number": 3, "player": "Animelib", "team": 9, "translation_type": 2}
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {
            "id": 77, "status": 21, "item_id": 5, "progress": "12:34", "meta": {"item_number": 3}
        }})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/bookmarks/77"))
        .and(query_param("media_type", "anime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"message": null}})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/manga/7965/chapters/229662/view"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"message": null}})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/anime/16133/players/309079/view"))
        .and(header("Site-Id", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"message": null}})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/api/anime/1--x/bookmark"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": null})))
        .mount(&s)
        .await;

    let c = client(&[&s]);
    // Битый элемент выбрасывается, следующая страница видна по ссылке.
    let query = libsocial::bookmarks_ext::BookmarkListQuery {
        user_id: 7,
        folder: Some(1),
        page: 1,
        ..Default::default()
    };
    let page = c.bookmark_list(Site::SlashLib, &query).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.has_next);

    assert!(c.bookmark(Site::AnimeLib, "1--x").await.unwrap().is_none());
    let place = libsocial::Place {
        item_id: 5,
        item_number: Some(3),
        progress: Some("12:34".into()),
        player: Some(libsocial::PlayerChoice {
            player: "Animelib".into(),
            team: 9,
            translation_type: 2,
        }),
    };
    let b = c.set_bookmark(Site::AnimeLib, "1--x", 21, Some(&place)).await.unwrap();
    assert_eq!(b.id, 77);
    c.delete_bookmark(Site::AnimeLib, 77).await.unwrap();
    c.mark_chapter_viewed(Site::MangaLib, 7965, 229662).await.unwrap();
    c.mark_episode_viewed(16133, 309079).await.unwrap();
}

#[tokio::test]
async fn comment_writes() {
    use libsocial::comments::{Comment, NewComment, Target, Vote};
    use libsocial::rich::Doc;

    let s = MockServer::start().await;
    let saved = || {
        ResponseTemplate::new(201).set_body_json(json!({"data": {
            "id": 9, "comment": "<p>ответ</p>", "relation_type": "chapter", "relation_id": 50,
            "parent_comment": 7, "root_id": 6, "comment_level": 2, "post_page": 3
        }}))
    };
    // Ответ на ответ: корень и уровень берутся от родителя, страница главы тоже.
    Mock::given(method("POST"))
        .and(path("/api/comments"))
        .and(body_partial_json(json!({
            "post_type": "chapter", "post_id": 50, "post_page": 3,
            "parent_comment": 7, "root_id": 6, "comment_level": 2,
            "comment": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "ответ"}]}]}
        })))
        .respond_with(saved())
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/comments/7/vote"))
        .and(body_partial_json(json!({"vote": 0})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/comments/9"))
        .respond_with(saved())
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/comments/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/comments/7/report"))
        .and(body_partial_json(
            json!({"reason": "3", "reportable_type": "comment", "reportable_id": 7}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .expect(1)
        .mount(&s)
        .await;

    let c = client(&[&s]);
    let parent: Comment = serde_json::from_value(json!({
        "id": 7, "root_id": 6, "parent_comment": 6, "comment_level": 1, "post_page": 3,
        "relation_type": "chapter", "relation_id": 50, "comment": "<p>родитель</p>"
    }))
    .unwrap();
    let new = NewComment {
        target: Target::Chapter {
            chapter_id: 50,
            page: Some(1),
        },
        text: Doc::from_plain("ответ"),
        reply_to: Some(parent),
        images: Vec::new(),
        sticker: None,
    };
    let posted = c.post_comment(&new).await.unwrap();
    assert_eq!(posted.id, 9);
    c.vote_comment(7, Vote::Down).await.unwrap();
    c.edit_comment(9, &Doc::from_plain("правка"), &[]).await.unwrap();
    c.delete_comment(9).await.unwrap();
    c.report_comment(7, "3", "").await.unwrap();
}

#[tokio::test]
async fn place_without_position_sends_no_progress() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/bookmarks"))
        .and(header("Site-Id", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 1, "status": 1}})))
        .expect(1)
        .mount(&s)
        .await;
    let place = libsocial::Place {
        item_id: 5,
        item_number: Some(2),
        progress: None,
        player: None,
    };
    client(&[&s])
        .set_bookmark(Site::RanobeLib, "1--n", 1, Some(&place))
        .await
        .unwrap();
    let sent: serde_json::Value = s.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(sent["bookmark"], json!({"status": 1, "item_id": 5}));
    assert_eq!(sent["media_type"], "manga");
}
