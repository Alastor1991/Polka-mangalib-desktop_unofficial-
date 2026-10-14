//! Запросы на запись вокруг тайтла на мок-сервере: путь, метод и тело.

use std::time::Duration;

use libsocial::comments::Vote;
use libsocial::media::{EntityKind, ReviewDraft, SimilarBy, TeamScore, TranslationScore, VoiceSubscription};
use libsocial::rich::Doc;
use libsocial::{Client, ClientConfig, Site};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(s: &MockServer) -> Client {
    Client::new(ClientConfig {
        api_mirrors: vec![format!("{}/api", s.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    })
    .unwrap()
}

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
}

async fn expect(s: &MockServer, verb: &str, route: &str, body: Option<serde_json::Value>, data: serde_json::Value) {
    let mock = Mock::given(method(verb)).and(path(route));
    let mock = match body {
        Some(b) => mock.and(body_partial_json(b)),
        None => mock,
    };
    mock.respond_with(ok(data)).expect(1).mount(s).await;
}

#[tokio::test]
async fn title_writes() {
    let s = MockServer::start().await;
    expect(
        &s,
        "POST",
        "/api/anime/rate",
        Some(json!({"score": 8, "rateable_id": 16133, "rateable_type": "anime"})),
        json!({"average": "8.5", "votes": 10, "user": 8}),
    )
    .await;
    expect(&s, "POST", "/api/chapters/50/like", None, json!({})).await;
    expect(
        &s,
        "POST",
        "/api/chapters/50/translation-rating",
        Some(json!({"translation_accuracy": 8, "readability_adaptation": 9, "editing_formatting": 7})),
        json!({"average": "8", "votes": 1, "can_rate": true}),
    )
    .await;
    expect(
        &s,
        "POST",
        "/api/branches/4666/subscribe",
        None,
        json!({"id": 4666, "notify": true}),
    )
    .await;
    expect(
        &s,
        "POST",
        "/api/anime/16133/subscriptions",
        Some(json!({"items": [{"team_id": 0, "translation_type": 2}]})),
        json!([{"team_id": 0, "translation_type": 2}]),
    )
    .await;
    expect(
        &s,
        "POST",
        "/api/favorites",
        Some(json!({"source_type": "people", "source_id": 93024})),
        json!({"is_subscribed": true, "source_type": "people", "source_id": 93024}),
    )
    .await;
    expect(
        &s,
        "POST",
        "/api/similar",
        Some(json!({"source_type": "manga", "source_id": 7965, "similar_id": 1283, "similar_type": "manga", "similar_by": [2, 3]})),
        json!({"id": 1, "similar": "по рисовке", "media": {"id": 1283, "name": "Fire Punch", "slug_url": "1283--fire-punch"}}),
    )
    .await;
    expect(&s, "POST", "/api/similar/1/vote", Some(json!({"vote": 1})), json!({})).await;

    let c = client(&s);
    assert_eq!(c.rate_title(Site::AnimeLib, 16133, 8).await.unwrap().user, 8);
    c.like_chapter(50).await.unwrap();
    let r = c
        .rate_translation(
            50,
            TranslationScore {
                translation_accuracy: 8,
                readability_adaptation: 9,
                editing_formatting: 7,
            },
        )
        .await
        .unwrap();
    assert!(r.can_rate);
    assert!(c.toggle_branch_subscription(Site::MangaLib, 4666).await.unwrap().notify);
    let subs = c
        .set_voice_subscriptions(
            Site::AnimeLib,
            16133,
            &[VoiceSubscription {
                team_id: 0,
                translation_type: Some(2),
            }],
        )
        .await
        .unwrap();
    assert_eq!(subs.len(), 1);
    assert!(
        c.toggle_favorite(EntityKind::People, 93024)
            .await
            .unwrap()
            .is_subscribed
    );
    let other: libsocial::models::TitleShort =
        serde_json::from_value(json!({"id": 1283, "name": "Fire Punch", "slug_url": "1283--fire-punch", "site": 1}))
            .unwrap();
    let added = c
        .add_similar(Site::MangaLib, 7965, &other, &[SimilarBy::Art, SimilarBy::Plot])
        .await
        .unwrap();
    c.vote_similar(added.id, Vote::Up).await.unwrap();
}

#[tokio::test]
async fn reviews_and_teams() {
    let s = MockServer::start().await;
    expect(
        &s,
        "POST",
        "/api/reviews",
        Some(json!({
            "title": "Заголовок", "evaluation": "positive", "status": "draft", "type": "short",
            "user_rating": [{"label": "Итоговая оценка", "value": 9}],
            "reviewable_type": "manga", "reviewable_id": 7965,
            "content": {"type": "doc"}
        })),
        json!({"id": 501}),
    )
    .await;
    expect(
        &s,
        "PUT",
        "/api/reviews/501",
        Some(json!({"status": "published"})),
        json!({}),
    )
    .await;
    expect(&s, "POST", "/api/review/501/vote", Some(json!({"vote": 0})), json!({})).await;
    expect(&s, "DELETE", "/api/reviews/501", None, json!({})).await;
    expect(
        &s,
        "POST",
        "/api/teams/587/rating",
        Some(json!({"translation": 8, "literacy": 9, "adaptation": 7, "formatting": 8, "stability": 6})),
        json!({}),
    )
    .await;
    expect(&s, "DELETE", "/api/teams/587/rating", None, json!({})).await;
    expect(
        &s,
        "POST",
        "/api/teams/587/ignore",
        None,
        json!({"ignored_by_user": true}),
    )
    .await;
    expect(
        &s,
        "DELETE",
        "/api/teams/587/ignore",
        None,
        json!({"ignored_by_user": false}),
    )
    .await;

    let c = client(&s);
    let mut draft = ReviewDraft {
        site: Site::MangaLib,
        title_id: 7965,
        title: "Заголовок".into(),
        text: Doc::from_plain("текст"),
        evaluation: "positive".into(),
        score: 9,
        full: false,
        draft: true,
    };
    let id = c.create_review(&draft).await.unwrap();
    assert_eq!(id, 501);
    draft.draft = false;
    c.update_review(id, &draft).await.unwrap();
    c.vote_review(id, Vote::Down).await.unwrap();
    c.delete_review(id).await.unwrap();
    c.rate_team(
        587,
        TeamScore {
            translation: 8,
            literacy: 9,
            adaptation: 7,
            formatting: 8,
            stability: 6,
        },
    )
    .await
    .unwrap();
    c.unrate_team(587).await.unwrap();
    c.ignore_team(587, true).await.unwrap();
    c.ignore_team(587, false).await.unwrap();
}
