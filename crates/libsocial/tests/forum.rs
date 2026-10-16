//! Форум: разделы, список тем, закреплённые, тема целиком и запросы на запись.
//! Сервер – wiremock, разделы – настоящий справочник, темы – синтетика по устройству
//! настоящего ответа.

use std::time::Duration;

use libsocial::forum::*;
use libsocial::rich::Block;
use libsocial::{Client, ClientConfig, Error, Site};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

async fn logged_in(server: &MockServer) -> Client {
    let c = client(server);
    c.set_token(Some(libsocial::Token {
        access_token: "t".into(),
        refresh_token: None,
        token_type: "Bearer".into(),
        expires_at: libsocial::auth::now_secs() + 3600,
        client_id: "1".into(),
    }))
    .await;
    c
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/forum_{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

#[tokio::test]
async fn categories_bare_and_wrapped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/forum/categories"))
        .respond_with(ok(fixture("categories")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/forum/categories"))
        .respond_with(ok(json!({"data": [
            {"id": 20, "name": "Вложенный", "parent_id": "5"},
            {"id": 21, "name": "Верхний", "parent_id": 0},
            "мусор"
        ]})))
        .mount(&server)
        .await;
    let c = client(&server);

    let categories = c.forum_categories().await.unwrap();
    assert_eq!(categories.len(), 13);
    let bugs = &categories[0];
    assert_eq!((bugs.id, bugs.slug.as_str(), bugs.order), (1, "bugs", 1));
    assert_eq!(bugs.icon.as_deref(), Some("bug"));
    assert!(categories.iter().all(ForumCategory::is_top_level));

    let wrapped = c.forum_categories().await.unwrap();
    assert_eq!(wrapped.len(), 2);
    assert!(!wrapped[0].is_top_level());
    assert!(wrapped[1].is_top_level());
}

#[tokio::test]
async fn list_is_a_flat_laravel_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion"))
        .and(query_param("page", "2"))
        .and(query_param("sort", "popular"))
        .and(query_param("category", "5"))
        .and(query_param("subscription", "0"))
        .and(query_param("author_subscription", "0"))
        .and(query_param("title", "тема"))
        .and(query_param_is_missing("user_id"))
        .respond_with(ok(fixture("list")))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let query = DiscussionQuery {
        page: 2,
        sort: DiscussionSort::Popular,
        section: ForumSection::Category(5),
        title: " тема ".into(),
        ..Default::default()
    };
    let page = c.discussions(&query).await.unwrap();

    // Номера страниц и адрес следующей лежат рядом с `data`, без `meta`.
    assert!(page.has_next);
    assert_eq!(
        (page.page, page.last_page, page.per_page, page.total),
        (Some(2), Some(5), Some(10), Some(47))
    );
    assert_eq!(page.items.len(), 2, "запись без id выброшена");

    let new = &page.items[0];
    assert_eq!((new.id, new.chatter_category_id, new.answered), (2002, Some(5), 3));
    assert!(new.category.is_none(), "пустой объект category – не раздел");
    assert_eq!(new.user.as_ref().unwrap().username, "user1");
    assert_eq!(new.like_count(), 2);
    assert!(new.body.is_string());
    assert_eq!(new.body_doc().plain_text(), "Новая тема");

    // Старая тема: текст Quill строкой, числа строками.
    let old = &page.items[1];
    assert_eq!((old.id, old.chatter_category_id, old.views), (2001, Some(13), 7));
    assert!(old.locked && old.yaoi);
    assert_eq!(old.body_doc().plain_text(), "Старая тема");
}

#[tokio::test]
async fn title_list_and_logged_in_filters() {
    let server = MockServer::start().await;
    // Список тайтла: только тайтл, порядок и страница, без фильтров подписок.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion"))
        .and(query_param("category", "all"))
        .and(query_param("source_type", "anime"))
        .and(query_param("source_id", "5"))
        .and(query_param("sort", "newest"))
        .and(query_param_is_missing("subscription"))
        .respond_with(ok(
            json!({"current_page": 1, "data": [], "last_page": 1, "next_page_url": null}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    // Подписки: раздел уходит как `all`.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion"))
        .and(query_param("category", "all"))
        .and(query_param("author_subscription", "1"))
        .respond_with(ok(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    // Свои темы.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion"))
        .and(query_param("category", "user"))
        .respond_with(ok(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;

    let title = DiscussionQuery {
        source: Some(DiscussionSource::Anime(5)),
        ..Default::default()
    };
    let followed = DiscussionQuery {
        section: ForumSection::Category(3),
        followed_authors: true,
        ..Default::default()
    };
    let mine = DiscussionQuery {
        section: ForumSection::Mine,
        ..Default::default()
    };

    // Гостю свои темы и подписки не предлагаются: отказ до запроса.
    let guest = client(&server);
    assert!(matches!(guest.discussions(&mine).await, Err(Error::Unauthorized)));
    assert!(matches!(guest.discussions(&followed).await, Err(Error::Unauthorized)));
    let page = guest.discussions(&title).await.unwrap();
    assert!(!page.has_next && page.items.is_empty());

    let c = logged_in(&server).await;
    c.discussions(&followed).await.unwrap();
    c.discussions(&mine).await.unwrap();
}

#[tokio::test]
async fn sticky_only_where_site_asks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/sticky"))
        .and(query_param("category", "5"))
        .and(query_param("subscription", "0"))
        .and(query_param("author_subscription", "0"))
        .respond_with(ok(fixture("sticky")))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;

    let sticky = c
        .sticky_discussions(&DiscussionQuery {
            section: ForumSection::Category(5),
            page: 4,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(sticky.len(), 1);
    assert_eq!((sticky[0].id, sticky[0].sticky), (1500, 1));
    assert!(sticky[0].body.is_null() && sticky[0].body_doc().blocks.is_empty());

    // Свои темы, подписки и список тайтла: сайт закреплённых не спрашивает.
    for query in [
        DiscussionQuery {
            section: ForumSection::Mine,
            ..Default::default()
        },
        DiscussionQuery {
            subscribed: true,
            ..Default::default()
        },
        DiscussionQuery {
            source: Some(DiscussionSource::Manga(1)),
            ..Default::default()
        },
    ] {
        assert!(c.sticky_discussions(&query).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn discussion_in_full() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/3001"))
        .respond_with(ok(fixture("discussion")))
        .mount(&server)
        .await;
    let c = client(&server);
    let d = c.discussion(3001).await.unwrap();

    assert_eq!((d.item.id, d.item.title.as_str()), (3001, "Тема целиком"));
    assert_eq!(d.item.chatter_category_id, Some(7));
    assert!(d.item.votes.liked);
    assert_eq!(d.item.like_count(), 4);
    assert!(d.subscribed && !d.can_toggle_lock);
    assert_eq!(d.post_id, Some(6001));
    assert_eq!(d.source, Some(DiscussionSource::Ranobe(77)));
    let relation = d.relation.as_ref().unwrap();
    assert_eq!(relation.source(), Some(DiscussionSource::Ranobe(77)));
    assert_eq!(relation.slug_url, "77--some-novel");
    assert!(relation.cover.as_ref().unwrap().best().is_some());

    // Текст – из `content`, картинка по имени вложения с адресом на хосте API.
    assert_eq!(d.content.plain_text(), "Текст темы");
    assert!(d.content_raw.is_object());
    let Block::Images(pictures) = &d.content.blocks[1] else {
        panic!("{:?}", d.content.blocks)
    };
    assert_eq!(
        pictures[0].url,
        format!("{}/uploads/forum/posts/3001/img-1.jpg", server.uri())
    );
    assert_eq!((pictures[0].width, pictures[0].height), (Some(736), Some(414)));
    assert_eq!(d.attachments.len(), 1);
    assert_eq!(d.attachments[0].name, "img-1");
}

#[tokio::test]
async fn old_discussion_text_and_other_shapes() {
    let server = MockServer::start().await;
    // Старая тема: без `content`, текст Quill в `post.body`, ответ в обёртке `data`.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/10"))
        .respond_with(ok(json!({"data": {"discussion": {
            "id": 10, "title": "Старая", "content": null, "votes": {"up": 1, "user": null},
            "post": {"id": 11, "body": {"ops": [{"insert": "Привет, "}, {"insert": "мир", "attributes": {"italic": true}}, {"insert": "\n"}]}, "attachments": []},
            "relation": null, "source_type": null, "source_id": null, "notification": false
        }}})))
        .mount(&server)
        .await;
    // Тема прямо в `data`, без `discussion`.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/12"))
        .respond_with(ok(
            json!({"data": {"id": 12, "title": "Без обёртки", "content": "<p>html</p>"}}),
        ))
        .mount(&server)
        .await;
    // Отказы приходят с кодом 200 и сообщением в `data.toast`.
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/13"))
        .respond_with(ok(
            json!({"data": {"toast": {"type": "error", "message": "Тема скрыта"}}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/14"))
        .respond_with(ok(json!({"data": {"toast": {"message": "Discussion not found"}}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/forum/discussion/15"))
        .respond_with(ok(json!({"discussion": null})))
        .mount(&server)
        .await;
    let c = client(&server);

    let old = c.discussion(10).await.unwrap();
    assert_eq!(old.content.plain_text(), "Привет, мир");
    assert!(old.content_raw.get("ops").is_some());
    assert_eq!((old.post_id, old.source, old.subscribed), (Some(11), None, false));
    assert!(!old.item.votes.liked);

    let bare = c.discussion(12).await.unwrap();
    assert_eq!(bare.content.plain_text(), "html");

    assert!(matches!(c.discussion(13).await, Err(Error::Other(m)) if m == "Тема скрыта"));
    assert!(matches!(c.discussion(14).await, Err(Error::NotFound)));
    assert!(matches!(c.discussion(15).await, Err(Error::NotFound)));
}

fn new_discussion() -> NewDiscussion {
    NewDiscussion {
        title: "Новая тема".into(),
        category_id: 5,
        yaoi: false,
        source: Some(DiscussionSource::Manga(7965)),
        text: libsocial::rich::Doc::from_plain("Текст"),
        images: vec![serde_json::from_value(json!({"name": "img-1", "filename": "img-1.jpg"})).unwrap()],
    }
}

fn new_discussion_body() -> Value {
    json!({
        "title": "Новая тема",
        "chatter_category_id": 5,
        "yaoi": false,
        "source_type": "manga",
        "source_id": 7965,
        "body": {"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "Текст"}]},
            {"type": "image", "attrs": {"description": "", "images": [{"image": "img-1"}]}}
        ]},
        "attachments": [{
            "name": "img-1", "filename": "img-1.jpg", "extension": null, "url": null,
            "previewUrl": null, "originalUrl": null, "width": null, "height": null
        }]
    })
}

#[tokio::test]
async fn create_and_edit() {
    let server = MockServer::start().await;
    for (reply, n) in [
        (json!({"discussion": {"id": 3005}}), 1),
        (json!({"data": {"id": "3006"}}), 1),
        (json!([]), 1),
        (json!({"data": {"toast": {"type": "error", "message": "Нет прав"}}}), 1),
    ] {
        Mock::given(method("POST"))
            .and(path("/api/forum/discussion"))
            .and(body_json(new_discussion_body()))
            .respond_with(ok(reply))
            .up_to_n_times(n)
            .expect(n)
            .mount(&server)
            .await;
    }
    Mock::given(method("PUT"))
        .and(path("/api/forum/discussion/3005"))
        .and(body_json(new_discussion_body()))
        .respond_with(ok(json!({"data": {}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/forum/discussion/3006"))
        .respond_with(ok(json!({"data": {"toast": {"message": "Тема закрыта"}}})))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;
    let new = new_discussion();

    // Ответ бывает `{discussion: {id}}`, `{id}` и пустым: тема уже создана, ошибки нет.
    assert_eq!(c.create_discussion(&new).await.unwrap(), Some(3005));
    assert_eq!(c.create_discussion(&new).await.unwrap(), Some(3006));
    assert_eq!(c.create_discussion(&new).await.unwrap(), None);
    // Отказ с кодом 200: id нет, есть `toast`.
    assert!(matches!(c.create_discussion(&new).await, Err(Error::Other(m)) if m == "Нет прав"));
    c.edit_discussion(3005, &new).await.unwrap();
    assert!(matches!(c.edit_discussion(3006, &new).await, Err(Error::Other(m)) if m == "Тема закрыта"));

    // Без названия и раздела запрос не уходит.
    let untitled = NewDiscussion {
        title: "  ".into(),
        ..new_discussion()
    };
    assert!(matches!(c.create_discussion(&untitled).await, Err(Error::Other(_))));
    let homeless = NewDiscussion {
        category_id: 0,
        ..new_discussion()
    };
    assert!(matches!(c.edit_discussion(3005, &homeless).await, Err(Error::Other(_))));
}

#[tokio::test]
async fn subscription_toggle() {
    let server = MockServer::start().await;
    for (id, reply) in [
        (1, json!({"data": true})),
        (2, json!(false)),
        (3, json!({"data": "1"})),
        (5, json!({"data": {"toast": {"message": "Discussion not found"}}})),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/api/forum/discussion/{id}/notification")))
            .and(body_json(json!({})))
            .respond_with(ok(reply))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/forum/discussion/4/notification"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;
    assert_eq!(c.toggle_discussion_subscription(1).await.unwrap(), Some(true));
    assert_eq!(c.toggle_discussion_subscription(2).await.unwrap(), Some(false));
    assert_eq!(c.toggle_discussion_subscription(3).await.unwrap(), Some(true));
    // Пустой ответ: подписка уже переключена, состояние перечитывают.
    assert_eq!(c.toggle_discussion_subscription(4).await.unwrap(), None);
    assert!(matches!(
        c.toggle_discussion_subscription(5).await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn like_replies() {
    let server = MockServer::start().await;
    let replies = [
        (1, json!({"parent": {"votes": {"up": 4, "user": 1}}})),
        (2, json!({"data": {"parent": {"likes": "3"}}})),
        (3, json!({"data": {"toast": {"message": "Нельзя лайкать свою тему"}}})),
        (4, json!({"data": {}})),
        // Лайк поставлен, а сообщение – лишь уведомление: ошибкой оно не становится.
        (
            5,
            json!({"data": {"parent": {"votes": {"up": 1, "user": 1}}, "toast": {"type": "success", "message": "Готово"}}}),
        ),
    ];
    for (id, reply) in replies {
        Mock::given(method("POST"))
            .and(path(format!("/api/discussion/{id}/vote")))
            .and(body_json(json!({"vote": 1})))
            .respond_with(ok(reply))
            .expect(1)
            .mount(&server)
            .await;
    }
    let c = logged_in(&server).await;
    assert_eq!(
        c.like_discussion(1).await.unwrap(),
        Some(LikeReply {
            up: 4,
            liked: Some(true)
        })
    );
    assert_eq!(
        c.like_discussion(2).await.unwrap(),
        Some(LikeReply { up: 3, liked: None })
    );
    assert!(matches!(c.like_discussion(3).await, Err(Error::Other(m)) if m.contains("свою тему")));
    assert_eq!(c.like_discussion(4).await.unwrap(), None);
    assert_eq!(c.like_discussion(5).await.unwrap().map(|r| r.up), Some(1));
}

#[tokio::test]
async fn images_go_to_the_default_upload_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/upload/image"))
        .respond_with(ok(
            json!({"data": [{"name": "img-1", "filename": "img-1.png", "url": "/uploads/x/img-1.png"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;
    let uploaded = c
        .upload_discussion_images(vec![("a.png".into(), b"\x89PNG".to_vec())])
        .await
        .unwrap();
    assert_eq!(uploaded[0].name, "img-1");

    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(body.contains(r#"name="content[]"; filename="a.png""#), "{body}");
    assert!(body.contains("image/png"));
    let site_id = requests[0].headers.get("site-id").unwrap().to_str().unwrap();
    assert_eq!(site_id, Site::MangaLib.id().to_string());
}

// ---------- живые проверки на чтение без входа ----------

fn live() -> Client {
    Client::new(ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn live_forum() {
    let c = live();
    let categories = c.forum_categories().await.unwrap();
    assert!(categories.iter().filter(|c| c.is_top_level()).count() > 5);

    let page = c.discussions(&DiscussionQuery::default()).await.unwrap();
    assert!(page.has_next, "плоский пагинатор: следующая страница по next_page_url");
    assert_eq!(page.page, Some(1));
    assert!(page.total.unwrap_or(0) > 1000);
    assert!(!page.items.is_empty());
    assert!(page.items.iter().any(|d| !d.body_doc().is_empty()));

    let sticky = c.sticky_discussions(&DiscussionQuery::default()).await.unwrap();
    assert!(sticky.iter().all(|d| d.sticky > 0));

    let first = &page.items[0];
    let details = c.discussion(first.id).await.unwrap();
    assert_eq!(details.item.id, first.id);
    assert_eq!(details.item.title, first.title);

    // Старая тема хранит текст в Quill, его разбор тоже должен давать текст.
    let old = c
        .discussions(&DiscussionQuery {
            page: 30000,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        old.items
            .iter()
            .any(|d| d.body.as_str().is_some_and(|s| s.contains("\"ops\"")))
    );
    assert!(old.items.iter().any(|d| !d.body_doc().is_empty()));
}
