//! Аккаунт и общение: уведомления, профиль, настройки, друзья, сообщения, подборки,
//! расширения закладок. Сервер – wiremock, ответы – синтетические фикстуры.

use std::time::Duration;

use libsocial::bookmarks_ext::*;
use libsocial::collections::*;
use libsocial::comments::Vote;
use libsocial::listing::SortOrder;
use libsocial::notifications::*;
use libsocial::social::*;
use libsocial::users::*;
use libsocial::{Client, ClientConfig, Error, Site};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/account_{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn ok(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(name))
}

fn empty_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"data": []}))
}

/// Запись должна прийти ровно один раз с таким методом, путём и телом. Пустой объект
/// значит «тело не проверяем» (запросы без тела).
async fn expect_write(server: &MockServer, verb: &str, p: &str, body: Value) {
    let mock = Mock::given(method(verb)).and(path(format!("/api/{p}")));
    let mock = if body == json!({}) {
        mock
    } else {
        mock.and(body_partial_json(body))
    };
    mock.respond_with(empty_ok()).expect(1).mount(server).await;
}

// ---------- уведомления ----------

#[tokio::test]
async fn notification_counts_parse() {
    let server = MockServer::start().await;
    Mock::given(path("/api/notifications/count"))
        .respond_with(ok("notification_count"))
        .mount(&server)
        .await;
    let counts = client(&server).notification_counts().await.unwrap();
    assert_eq!(counts.unread.all, 2);
    assert_eq!(counts.unread.get(NotificationTab::Comments), 1);
    assert_eq!(counts.all.get(NotificationTab::Chapter), 7);
}

#[tokio::test]
async fn notifications_send_required_params_and_parse() {
    let server = MockServer::start().await;
    Mock::given(path("/api/notifications"))
        .and(query_param("notification_type", "comments"))
        .and(query_param("read_type", "unread"))
        .and(query_param("sort_type", "asc"))
        .and(query_param("page", "2"))
        .and(query_param("q", "глава"))
        .respond_with(ok("notifications"))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .notifications(&NotificationsQuery {
            tab: NotificationTab::Comments,
            read: ReadFilter::Unread,
            order: SortOrder::Asc,
            page: 2,
            text: "глава".into(),
            exact_type: None,
        })
        .await
        .unwrap();
    // Пятая запись не разобралась и выброшена, остальные на месте.
    assert_eq!(page.items.len(), 4);
    assert!(page.has_next);
    assert_eq!(page.total, Some(52));
    assert_eq!(page.page, Some(1));

    let first = &page.items[0];
    assert_eq!(first.text().plain_text().trim(), "Вышла новая глава");
    assert_eq!(first.site(), Some(Site::SlashLib));
    match first.event() {
        NotificationEvent::Chapter(c) => {
            assert!(c.opens_title());
            assert_eq!(c.media.unwrap().display_name(), "Название");
            assert_eq!(c.teams[0].name, "Команда");
        }
        other => panic!("{other:?}"),
    }
    match page.items[1].event() {
        NotificationEvent::Comment(c) => assert_eq!(c.comment.unwrap().relation_id, Some(4014248)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        page.items[2].event(),
        NotificationEvent::Friend {
            event: FriendEvent::Request,
            ..
        }
    ));
    assert!(matches!(page.items[3].event(), NotificationEvent::Other { .. }));
    assert!(page.items[3].text().blocks.is_empty());
}

#[tokio::test]
async fn notification_writes() {
    let server = MockServer::start().await;
    expect_write(&server, "PUT", "notifications/42", json!({})).await;
    expect_write(
        &server,
        "PUT",
        "notifications/bulk",
        json!({"notification_type": "chapter"}),
    )
    .await;
    expect_write(&server, "DELETE", "notifications/43", json!({})).await;
    Mock::given(method("DELETE"))
        .and(path("/api/notifications/bulk"))
        .and(body_json(json!({"notification_type": "all", "is_read": true})))
        .respond_with(empty_ok())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/notifications/bulk"))
        .and(body_json(json!({"notification_type": "card", "is_read": null})))
        .respond_with(empty_ok())
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    c.mark_notification_read(42).await.unwrap();
    c.mark_notifications_read(NotificationTab::Chapter).await.unwrap();
    c.delete_notification(43).await.unwrap();
    c.delete_notifications(NotificationTab::All, ReadFilter::Read)
        .await
        .unwrap();
    c.delete_notifications(NotificationTab::Card, ReadFilter::All)
        .await
        .unwrap();
}

// ---------- профиль и статистика ----------

#[tokio::test]
async fn profile_requests_fields_and_parses() {
    let server = MockServer::start().await;
    Mock::given(path("/api/user/1001"))
        .and(query_param("fields[]", "about"))
        .respond_with(ok("profile"))
        .mount(&server)
        .await;
    Mock::given(path("/api/user/1002"))
        .respond_with(ok("profile_closed"))
        .mount(&server)
        .await;
    Mock::given(path("/api/user/9"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let c = client(&server);
    let p = c.user_profile(1001, &ProfileField::PROFILE_PAGE).await.unwrap();
    assert_eq!(p.user.username, "user1");
    assert_eq!(
        p.user.avatar_url(),
        Some("https://cover.cdnlibs.org/uploads/users/1001/x.jpg")
    );
    assert_eq!(p.about_text().plain_text().trim(), "Текст о себе");
    assert_eq!(p.points_info.as_ref().unwrap().level, 8);
    assert_eq!(p.teams[0].name, "Команда");
    assert_eq!(p.user.login_streak.as_ref().unwrap().max_login_streak, 21);
    assert!(!p.is_banned());

    let closed = c.user_profile(1002, &[]).await.unwrap();
    assert!(!closed.user.can_view_profile);
    assert!(closed.user.avatar_url().is_none());
    assert!(closed.points_info.is_none() && closed.teams.is_empty());

    assert!(matches!(c.user_profile(9, &[]).await, Err(Error::NotFound)));
}

#[tokio::test]
async fn stats_and_view_statistics() {
    let server = MockServer::start().await;
    Mock::given(path("/api/user/1001/stats"))
        .and(header("Site-Id", "5"))
        .respond_with(ok("stats"))
        .mount(&server)
        .await;
    Mock::given(path("/api/user/1001/view-statistics"))
        .and(query_param("site_ids[]", "1"))
        .respond_with(ok("view_statistics"))
        .mount(&server)
        .await;
    Mock::given(path("/api/user/1002/view-statistics"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": ""})))
        .mount(&server)
        .await;
    let c = client(&server);
    let stats = c.user_stats(Site::AnimeLib, 1001).await.unwrap();
    assert_eq!(stats.genres[0].percent, Some(25.5));
    assert_eq!(stats.comments.unwrap().value, 74);
    assert_eq!(stats.reviews.unwrap().evaluation["all"], 0);

    let v = c.user_view_statistics(1001, &[Site::MangaLib]).await.unwrap();
    assert_eq!(v.totals.manga_chapters, 15);
    assert_eq!(v.time.total, "1 час 45 минут");
    assert_eq!(v.periods.last_7_days.time.manga_minutes, 105);
    assert_eq!(v.charts["last_7_days"].datasets[0].data, vec![0, 3]);
    assert!(v.charts["last_30_days"].labels.is_empty());
    assert_eq!(v.minutes_per_item["anime"], 20);

    assert!(matches!(c.user_view_statistics(1002, &[]).await, Err(Error::Forbidden)));
}

#[tokio::test]
async fn histories_use_their_paths_and_sites() {
    let server = MockServer::start().await;
    Mock::given(path("/api/user/chapters/history"))
        .and(header("Site-Id", "2"))
        .and(query_param("page", "1"))
        .respond_with(ok("history_chapters"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/user/chapters/players/history"))
        .respond_with(ok("history_chapters"))
        .expect(1)
        .mount(&server)
        .await;
    // Для аниме сайт всегда AnimeLib, что бы ни передали.
    Mock::given(path("/api/user/players/history"))
        .and(header("Site-Id", "5"))
        .respond_with(ok("history_anime"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let read = c.view_history(HistoryKind::Chapters, Site::SlashLib, 1).await.unwrap();
    assert_eq!(read.items.len(), 1);
    assert!(read.has_next && read.total.is_none());
    let item = read.items[0].item.as_ref().unwrap();
    assert_eq!(
        (item.number.as_str(), item.name.clone(), item.branch_id),
        ("1", None, None)
    );
    assert_eq!(read.items[0].media.display_name(), "Человек-бензопила");

    c.view_history(HistoryKind::ChapterPlayers, Site::MangaLib, 0)
        .await
        .unwrap();

    let anime = c.view_history(HistoryKind::Anime, Site::MangaLib, 1).await.unwrap();
    assert!(!anime.has_next);
    let ep = anime.items[0].item.as_ref().unwrap();
    assert_eq!(
        (ep.model.as_str(), ep.season.as_deref(), ep.anime_id),
        ("episode", Some("1"), Some(16133))
    );
    assert_eq!(ep.players.len(), 1);
}

// ---------- настройки ----------

#[tokio::test]
async fn privacy_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/settings/privacy"))
        .respond_with(ok("privacy"))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "PUT",
        "user/settings/privacy",
        json!({
            "profile_visibility": 1, "statistics_visibility": 3, "statistics_site_ids": [1, 2, 3, 4, 5],
            "previous_usernames_visibility": 0, "previous_usernames": []
        }),
    )
    .await;
    let c = client(&server);
    let mut s = c.privacy_settings().await.unwrap();
    assert_eq!(s.statistics_site_ids, vec![1, 2, 3, 4, 5]);
    assert_eq!(s.previous_usernames[0].username, "old1");
    s.profile_visibility = Visibility::FriendsOnly;
    s.forget_username(1);
    c.set_privacy_settings(&s).await.unwrap();
}

#[tokio::test]
async fn notification_settings_are_sent_whole() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/settings/notifications"))
        .respond_with(ok("notify_settings"))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "PUT",
        "user/settings/notifications",
        json!({
            "user_id": 1001, "manga": true, "disable_old_comments_notif": 30, "reading": true,
            "push_forum": true, "notify_anime": true
        }),
    )
    .await;
    let c = client(&server);
    let mut s = c.notification_settings().await.unwrap();
    assert_eq!(s.disable_old_comments_notif, None);
    s.manga = true;
    s.disable_old_comments_notif = Some(30);
    c.set_notification_settings(&s).await.unwrap();
}

#[tokio::test]
async fn content_filter_replaces_whole_set() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/1001/content-filter"))
        .respond_with(ok("content_filter"))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "POST",
        "user/1001/content-filter",
        json!({"filters": [{"value": 123, "type": "tag", "site_id": 1}, {"value": 5, "type": "genre", "site_id": 1}]}),
    )
    .await;
    let c = client(&server);
    let rules = c.content_filter(1001).await.unwrap();
    // Правило неизвестного вида выброшено.
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].kind, FilterKind::Tag);
    c.set_content_filter(1001, &rules).await.unwrap();
}

#[tokio::test]
async fn personalization_roundtrip() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/1001/personalization-settings"))
        .respond_with(ok("personalization"))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "POST",
        "user/1001/personalization-settings",
        json!({
            "latest_updates": "all", "grid_cards_count": "auto", "disable_autobookmark": true,
            "start_page_blocks": [{"id": "popular-updates", "visible": true}, {"id": "latest-views", "visible": false}],
            "bottom_menu_order": ["bookmarks", "catalog", "home", "notifications", "menu"]
        }),
    )
    .await;
    let c = client(&server);
    let mut s = c.personalization_settings(1001).await.unwrap();
    assert_eq!(s.start_page_blocks.len(), 2);
    assert_eq!(s.disable_autobookmark, Some(false));
    s.latest_updates = Some("all".into());
    s.disable_autobookmark = Some(true);
    c.set_personalization_settings(1001, &s).await.unwrap();
}

// ---------- друзья, игнор, сообщения ----------

#[tokio::test]
async fn friends_lists_and_status() {
    let server = MockServer::start().await;
    Mock::given(path("/api/friendship"))
        .and(query_param("status", "1"))
        .and(query_param("user_id", "1001"))
        .and(query_param_is_missing("sender"))
        .respond_with(ok("friends"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/friendship"))
        .and(query_param("status", "0"))
        .and(query_param("sender", "1"))
        .respond_with(ok("friends"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/friendship/1002/mutual"))
        .respond_with(empty_ok())
        .mount(&server)
        .await;
    Mock::given(path("/api/friendship/1003"))
        .respond_with(ok("friendship_none"))
        .mount(&server)
        .await;
    expect_write(&server, "PUT", "friendship/bulk", json!({"status": 1})).await;
    expect_write(&server, "PUT", "friendship/bulk", json!({"status": 2})).await;
    let c = client(&server);
    let friends = c.friends(1001, FriendList::Friends, "", 1).await.unwrap();
    assert_eq!(friends.items[0].user.as_ref().unwrap().username, "user2");
    assert!(friends.items[0].status.is_friend && !friends.has_next);
    c.friends(1001, FriendList::Outgoing, "", 1).await.unwrap();
    assert!(c.mutual_friends(1002, 1).await.unwrap().items.is_empty());
    let none = c.friendship_with(1003).await.unwrap();
    assert!(none.id.is_none() && none.user.is_none());
    assert_eq!(none.status.available_action(), FriendshipAction::Request);
    c.friendship_bulk(FriendBulk::AcceptAll).await.unwrap();
    c.friendship_bulk(FriendBulk::DenyAll).await.unwrap();
}

#[tokio::test]
async fn ignore_list_and_writes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ignore"))
        .and(query_param("sourceable_type", "user"))
        .and(query_param("sourceable_id", "1001"))
        .respond_with(ok("ignore_list"))
        .mount(&server)
        .await;
    Mock::given(path("/api/ignore/1004"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"id": 77, "comment": "причина", "status": {"is_blocked": true, "blocked_by_user": false}}}),
        ))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "POST",
        "ignore",
        json!({"sourceable_id": 1001, "sourceable_type": "user", "user_id": 1004, "comment": "причина"}),
    )
    .await;
    expect_write(&server, "PUT", "ignore/77", json!({"comment": "новый"})).await;
    expect_write(&server, "DELETE", "ignore/77", json!({})).await;
    let c = client(&server);
    let list = c.ignore_list(IgnoreSource::User(1001), "", 1).await.unwrap();
    assert_eq!(list.items[0].comment.as_deref(), Some("причина"));
    assert_eq!(list.total, Some(1));
    let st = c.ignore_status(1004).await.unwrap();
    assert!(st.status.is_blocked && st.id == Some(77));
    c.ignore_add(IgnoreSource::User(1001), 1004, "причина").await.unwrap();
    c.ignore_update(77, "новый").await.unwrap();
    c.ignore_remove(77).await.unwrap();
    // Команде нужна причина: запрос не уходит.
    assert!(matches!(
        c.ignore_add(IgnoreSource::Team(5), 1004, " ").await,
        Err(Error::Other(_))
    ));
}

#[tokio::test]
async fn message_threads_and_messages() {
    let server = MockServer::start().await;
    Mock::given(path("/api/messenger/threads"))
        .and(method("GET"))
        .and(query_param("archived", "0"))
        .respond_with(ok("threads"))
        .mount(&server)
        .await;
    Mock::given(path("/api/messenger/threads/find/1002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": fixture("threads")["data"][0]})))
        .mount(&server)
        .await;
    Mock::given(path("/api/messenger/threads/find/1999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"data": {"toast": {"message": "x"}}})))
        .mount(&server)
        .await;
    Mock::given(path("/api/messenger/threads/9001/messages"))
        .and(method("GET"))
        .respond_with(ok("messages"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/messenger/threads"))
        .and(body_json(json!({"participants": [1002]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": fixture("threads")["data"][0]})))
        .expect(1)
        .mount(&server)
        .await;
    let message = json!({"data": fixture("messages")["data"][0]});
    Mock::given(method("POST"))
        .and(path("/api/messenger/threads/9001/messages"))
        .and(body_json(json!({"body": "привет"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(message.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/messenger/threads/9001/messages/70002"))
        .and(body_json(json!({"body": "правка"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(message))
        .expect(1)
        .mount(&server)
        .await;
    expect_write(
        &server,
        "DELETE",
        "messenger/threads/9001/messages",
        json!({"messages": [70001, 70002]}),
    )
    .await;

    let c = client(&server);
    let threads = c.message_threads(false, "", 1).await.unwrap();
    assert!(threads.has_next);
    assert_eq!(threads.items[0].title(), "user2");
    assert_eq!(threads.items[0].unread(), 2);
    assert!(threads.items[0].last_message.as_ref().unwrap().is_edited());
    assert!(threads.items[1].last_message.is_none());

    assert_eq!(c.find_message_thread(1002).await.unwrap().unwrap().id, 9001);
    assert!(c.find_message_thread(1999).await.unwrap().is_none());

    // Короткий диалог приходит без `meta` и `links`.
    let msgs = c.thread_messages(9001, 1).await.unwrap();
    assert_eq!(msgs.items.len(), 2);
    assert!(!msgs.has_next);
    assert_eq!(msgs.items[0].index, 2);

    assert_eq!(c.create_message_thread(1002).await.unwrap().id, 9001);
    c.send_thread_message(9001, "  привет ").await.unwrap();
    c.edit_thread_message(9001, 70002, "правка").await.unwrap();
    c.delete_thread_messages(9001, &[70001, 70002]).await.unwrap();
    assert!(c.send_thread_message(9001, "").await.is_err());
    assert!(
        c.send_thread_message(9001, &"я".repeat(MESSAGE_MAX_CHARS + 1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn check_online_maps_ids() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat/check-online"))
        .and(body_json(json!({"users": [1001, 1002]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"1001": "online", "1002": "offline"}})))
        .expect(1)
        .mount(&server)
        .await;
    let online = client(&server).check_online(&[1001, 1002]).await.unwrap();
    assert!(online[&1001]);
    assert!(!online[&1002]);
}

// ---------- подборки ----------

#[tokio::test]
async fn collections_list_clamps_limit() {
    let server = MockServer::start().await;
    Mock::given(path("/api/collections"))
        .and(query_param("limit", "12"))
        .and(query_param("sort_by", "popular"))
        .and(query_param("sort_type", "desc"))
        .and(query_param("site_ids[]", "1"))
        .and(query_param("type", "titles"))
        .respond_with(ok("collections"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/collections"))
        .and(query_param("user_id", "1001"))
        .and(query_param("subscriptions", "1"))
        .and(query_param("limit", "24"))
        .respond_with(ok("collections"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let page = c
        .collections(&CollectionsQuery {
            limit: 5,
            sort: Some(CollectionSort::Popular),
            order: Some(SortOrder::Desc),
            sites: vec![Site::MangaLib],
            kind: Some(CollectionKind::Titles),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.has_next);
    assert_eq!(page.items[0].votes.user, None);
    assert_eq!(page.items[1].votes.user, Some(Vote::Down));
    assert_eq!(page.items[0].previews.len(), 1);
    assert_eq!(page.items[0].site(), Some(Site::MangaLib));

    c.collections(&CollectionsQuery {
        limit: 99,
        favorites: true,
        user_id: Some(1001),
        ..Default::default()
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn one_collection_with_blocks() {
    let server = MockServer::start().await;
    Mock::given(path("/api/collections/19978"))
        .respond_with(ok("collection"))
        .mount(&server)
        .await;
    Mock::given(path("/api/collections/1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let c = client(&server);
    let col = c.collection(19978).await.unwrap();
    assert_eq!(col.brief.name, "Название подборки");
    assert_eq!(col.description_doc().plain_text().trim(), "Описание");
    assert_eq!(col.user.as_ref().unwrap().username, "user1");
    let items = &col.blocks[0].items;
    assert_eq!(items[0].title().unwrap().slug_url, "7965--chainsaw-man");
    assert!(items[1].title().is_none());
    assert_eq!(items[1].related.as_ref().unwrap()["name"], "Персонаж");
    assert!(matches!(c.collection(1).await, Err(Error::NotFound)));
}

#[tokio::test]
async fn collection_writes() {
    let server = MockServer::start().await;
    let saved = ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 555}}));
    let body = json!({
        "type": "titles", "name": "Моя", "interactive": false, "private": false, "spoiler": true, "adult": false,
        "description": null, "attachments": [],
        "blocks": [{"uuid": "u-1", "name": "Блок", "items": [
            {"item_id": 7965, "item_type": "manga", "comment": "да", "order": 1}]}]
    });
    Mock::given(method("POST"))
        .and(path("/api/collections"))
        .and(body_partial_json(body.clone()))
        .respond_with(saved.clone())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/collections/555"))
        .and(body_partial_json(body))
        .respond_with(saved)
        .expect(1)
        .mount(&server)
        .await;
    expect_write(&server, "DELETE", "collections/555", json!({})).await;
    expect_write(&server, "POST", "collection/555/vote", json!({"vote": 1})).await;
    let c = client(&server);
    let mut draft = CollectionDraft::new(CollectionKind::Titles, "Моя");
    draft.spoiler = true;
    draft.blocks.push(DraftBlock {
        uuid: "u-1".into(),
        name: "Блок".into(),
        items: vec![DraftItem {
            item_id: 7965,
            item_type: "manga".into(),
            comment: "да".into(),
        }],
    });
    assert_eq!(c.create_collection(&draft).await.unwrap(), 555);
    assert_eq!(c.update_collection(555, &draft).await.unwrap(), 555);
    c.delete_collection(555).await.unwrap();
    c.vote_collection(555, Vote::Up).await.unwrap();
}

// ---------- расширения закладок ----------

#[tokio::test]
async fn bookmark_list_with_sorting() {
    let server = MockServer::start().await;
    Mock::given(path("/api/bookmarks"))
        .and(header("Site-Id", "5"))
        .and(query_param("user_id", "1001"))
        .and(query_param("status", "0"))
        .and(query_param("sort_by", "last_episode_at"))
        .and(query_param("sort_type", "asc"))
        .and(query_param("q", "ман"))
        .respond_with(ok("bookmarks"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let page = c
        .bookmark_list(
            Site::AnimeLib,
            &BookmarkListQuery {
                user_id: 1001,
                sort: Some(BookmarkSort::LastItemAt),
                order: Some(SortOrder::Asc),
                text: "ман".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.has_next);
    assert_eq!(page.items.len(), 2);
    let full = &page.items[0];
    assert_eq!(full.bookmark_type(), Some(BookmarkType::Manga));
    assert_eq!(full.meta.comment.as_deref(), Some("мой комментарий"));
    assert_eq!(full.meta.rewatches_history[0].start, "2026-01-01");
    assert_eq!(full.meta.notes, 2);
    assert_eq!(full.rating, Some(8.0));
    let bare = &page.items[1];
    assert!(bare.meta.comment.is_none() && bare.item_id.is_none() && bare.progress.is_none());
    assert_eq!(bare.rating, Some(7.5));
    assert_eq!(bare.media.as_ref().unwrap().slug_url, "7965--chainsaw-man");
}

#[tokio::test]
async fn bookmark_folder_management() {
    let server = MockServer::start().await;
    let folder = ResponseTemplate::new(200).set_body_json(json!({"data": fixture("folders")["data"][1]}));
    Mock::given(method("GET"))
        .and(path("/api/bookmarks/folder/1001"))
        .respond_with(ok("folders"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/bookmarks/folder"))
        .and(header("Site-Id", "5"))
        .and(body_json(json!({"name": "Новая"})))
        .respond_with(folder.clone())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/bookmarks/folder/51"))
        .and(body_json(json!({"name": "Иначе", "notify": true})))
        .respond_with(folder)
        .expect(1)
        .mount(&server)
        .await;
    expect_write(&server, "DELETE", "bookmarks/folder/51", json!({"move_to": 1})).await;
    Mock::given(method("DELETE"))
        .and(path("/api/bookmarks/folder/52"))
        .and(body_json(json!({})))
        .respond_with(empty_ok())
        .expect(1)
        .mount(&server)
        .await;
    expect_write(&server, "PUT", "bookmarks/folder/order", json!({"order": [2, 1, 51]})).await;
    expect_write(
        &server,
        "PUT",
        "bookmarks/folder/notifications",
        json!({"folders": [{"id": 1, "notify": true}, {"id": 2, "notify": false}]}),
    )
    .await;

    let c = client(&server);
    let folders = c.bookmark_folders(1001).await.unwrap();
    assert!(folders[0].public && !folders[0].is_custom() && folders[0].is_for(Site::SlashLib));
    assert_eq!(folders[0].text_color.as_deref(), Some("#fff"));
    assert!(folders[1].is_custom() && folders[1].order.is_none() && folders[1].notify);

    c.create_bookmark_folder(Site::AnimeLib, " Новая ").await.unwrap();
    assert!(c.create_bookmark_folder(Site::AnimeLib, " ").await.is_err());
    c.update_bookmark_folder(
        Site::AnimeLib,
        51,
        &FolderUpdate {
            name: Some("Иначе".into()),
            notify: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    c.delete_bookmark_folder(Site::MangaLib, 51, Some(1)).await.unwrap();
    c.delete_bookmark_folder(Site::MangaLib, 52, None).await.unwrap();
    c.reorder_bookmark_folders(Site::MangaLib, &[2, 1, 51]).await.unwrap();
    c.set_bookmark_folder_notifications(Site::MangaLib, &[(1, true), (2, false)])
        .await
        .unwrap();
}

#[tokio::test]
async fn bookmark_bulk_operations() {
    let server = MockServer::start().await;
    expect_write(
        &server,
        "POST",
        "bookmarks/bulk/store",
        json!({"media_type": "manga", "media_ids": [7965, 123], "status": 2}),
    )
    .await;
    expect_write(
        &server,
        "PUT",
        "bookmarks/bulk",
        json!({"media_type": "anime", "media_ids": [16133], "status": 22}),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path("/api/bookmarks/bulk"))
        .and(body_json(json!({"media_type": "manga", "media_ids": [7965]})))
        .respond_with(empty_ok())
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    c.bookmarks_bulk_add(Site::MangaLib, &[7965, 123], 2).await.unwrap();
    c.bookmarks_bulk_move(Site::AnimeLib, &[16133], 22).await.unwrap();
    c.bookmarks_bulk_remove(Site::SlashLib, &[7965]).await.unwrap();
    // Пустой список запроса не создаёт: у мок-сервера нет ожидания на лишние вызовы.
    c.bookmarks_bulk_remove(Site::MangaLib, &[]).await.unwrap();
}

#[tokio::test]
async fn bookmark_notes() {
    let server = MockServer::start().await;
    Mock::given(path("/api/bookmarks/note"))
        .and(method("GET"))
        .and(query_param("bookmark_id", "1000000001"))
        .and(query_param("bookmark_type", "manga_bookmark"))
        .respond_with(ok("notes"))
        .mount(&server)
        .await;
    Mock::given(path("/api/bookmarks/note/chapter-1000000001"))
        .and(query_param("number_first", "1"))
        .and(query_param("number_secondary", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": fixture("notes")["data"][0]})))
        .mount(&server)
        .await;
    Mock::given(path("/api/bookmarks/note/chapter-1000000002"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"data": {"toast": {"message": "Not Found"}}})))
        .mount(&server)
        .await;
    expect_write(
        &server,
        "POST",
        "bookmarks/note",
        json!({
            "bookmark_id": 1000000001_u64, "bookmark_type": "manga_bookmark", "item_id": 1766671,
            "item_type": "chapter", "number_first": "1", "number_secondary": "1",
            "content": [{"position": 5, "text": "текст"}]
        }),
    )
    .await;
    expect_write(
        &server,
        "DELETE",
        "bookmarks/note/3001",
        json!({"bookmark_id": 1000000001_u64, "bookmark_type": "manga_bookmark"}),
    )
    .await;
    let c = client(&server);
    let list = c.bookmark_notes(1000000001, BookmarkType::Manga, 1).await.unwrap();
    assert_eq!(list.items[0].notes.len(), 2);
    assert_eq!(list.items[0].notes[1].position, "7");
    assert_eq!(list.items[0].number.secondary, "1");

    let one = c
        .bookmark_note(1000000001, BookmarkType::Manga, "1", "1")
        .await
        .unwrap();
    assert_eq!(one.unwrap().id, Some(3001));
    assert!(
        c.bookmark_note(1000000002, BookmarkType::Manga, "1", "1")
            .await
            .unwrap()
            .is_none()
    );

    let note = NoteSave {
        bookmark_id: 1000000001,
        kind: BookmarkType::Manga,
        item_id: 1766671,
        number_secondary: "1".into(),
        number_first: "1".into(),
        entries: vec![NoteEntry {
            position: "5".into(),
            text: "текст".into(),
        }],
    };
    c.save_bookmark_note(&note).await.unwrap();
    c.delete_bookmark_note(3001, 1000000001, BookmarkType::Manga)
        .await
        .unwrap();
}

#[tokio::test]
async fn bookmark_meta_keeps_place() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/bookmarks"))
        .and(body_json(json!({
            "media_type": "manga", "media_slug": "7965--chainsaw-man",
            "bookmark": {"status": 1},
            "meta": {"comment": "заметка", "rewatches_history": [
                {"start": "2026-01-01", "end": "2026-02-01"}, {"start": "2026-10-01", "end": null}]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": 1000000001_u64, "status": 1}})))
        .expect(1)
        .mount(&server)
        .await;
    let b = client(&server)
        .update_bookmark_meta(
            Site::MangaLib,
            "7965--chainsaw-man",
            1,
            &BookmarkMetaUpdate {
                comment: Some(" заметка ".into()),
                rewatches_history: Some(vec![
                    Rewatch::new("2026-01-01", Some("2026-02-01".into())),
                    Rewatch::new("2026-10-01", None),
                ]),
            },
        )
        .await
        .unwrap();
    assert_eq!(b.id, 1000000001);
}

// ---------- живые проверки: только чтение без входа ----------
//
// POLKA_INTERFACE=enp4s0 cargo test -p libsocial --test account -- --ignored --test-threads=1

fn live_client() -> Client {
    Client::new(ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        api_interval: Duration::from_millis(1100),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn live_collections() {
    let c = live_client();
    let page = c
        .collections(&CollectionsQuery {
            sort: Some(CollectionSort::Newest),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!page.items.is_empty());
    let first = page.items[0].id;
    let col = c.collection(first).await.unwrap();
    assert_eq!(col.brief.id, first);
    assert!(!col.blocks.is_empty());
}

#[tokio::test]
#[ignore]
async fn live_users_and_profile() {
    let c = live_client();
    let top = c
        .users(
            &libsocial::search::UserQuery {
                filter: libsocial::search::UserFilter::Points,
                ..Default::default()
            },
            1,
        )
        .await
        .unwrap();
    assert!(top.items.len() > 50);
    let id = top.items[0].id;
    let profile = c.user_profile(id, &ProfileField::PROFILE_PAGE).await.unwrap();
    assert_eq!(profile.user.id, id);
    assert!(profile.points_info.is_some());
    // Статистика открытого профиля читается без входа, закрытая даёт 403.
    match c.user_stats(Site::MangaLib, id).await {
        Ok(_) | Err(Error::Forbidden) => {}
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
#[ignore]
async fn live_notifications_need_login() {
    // Без входа ленты уведомлений нет: сервер просит войти.
    let c = live_client();
    assert!(matches!(c.notification_counts().await, Err(Error::Unauthorized)));
}
