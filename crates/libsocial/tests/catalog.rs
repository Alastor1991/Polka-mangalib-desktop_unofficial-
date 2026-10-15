//! Каталог, справочник, ленты, новости и поиск. Сервер – wiremock, ответы – синтетические
//! фикстуры по устройству настоящих. Живые проверки (`#[ignore]`) ходят в настоящий API
//! запросами на чтение без входа:
//!
//! ```sh
//! POLKA_INTERFACE=enp4s0 cargo test -p libsocial --test catalog -- --ignored --test-threads=1
//! ```

use std::time::Duration;

use libsocial::catalog::{Bounds, CatalogFilter, CatalogSort, RandomCursor, SortOrder, Target, TargetModel};
use libsocial::feeds::{NewsFilter, NewsKind, TopGroup, TopTime};
use libsocial::rich::Block;
use libsocial::search::{EntityKind, EntityQuery, EntitySort, UserFilter, UserQuery};
use libsocial::{Client, ClientConfig, Error, Site};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::new(ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    })
    .unwrap()
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn ok(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(name))
}

async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("/api/{route}")))
        .respond_with(response)
        .mount(server)
        .await;
}

// --- каталог ---------------------------------------------------------------------------

#[tokio::test]
async fn manga_catalog_sends_filter_and_parses_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/manga"))
        .and(header("Site-Id", "1"))
        .and(query_param("genres[]", "34"))
        .and(query_param("genres_exclude[]", "5"))
        .and(query_param("genres_soft_search", "1"))
        .and(query_param("sort_by", "views"))
        .and(query_param("site_id[]", "3"))
        .and(query_param("fields[]", "rate_avg"))
        .and(query_param("page", "1"))
        .respond_with(ok("catalog_manga.json"))
        .expect(1)
        .mount(&server)
        .await;

    let filter = CatalogFilter {
        genres: vec![34],
        genres_exclude: vec![5],
        genres_any: true,
        site_ids: vec![Site::RanobeLib.id()],
        sort: CatalogSort::Views,
        ..Default::default()
    };
    let page = client(&server)
        .catalog_filtered(Site::MangaLib, &filter, 1)
        .await
        .unwrap();
    // Негодная запись выброшена, остальные на месте.
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].slug_url, "7965--chainsaw-man");
    assert!(page.items[0].rating.as_ref().unwrap().average > 9.0);
    assert!(page.has_next);
    assert_eq!(page.per_page, Some(60));
}

#[tokio::test]
async fn anime_catalog_uses_links_next() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/anime"))
        .and(header("Site-Id", "5"))
        .and(query_param("episodes_min", "12"))
        .and(query_param("sort_by", "episodes_count"))
        .and(query_param("sort_type", "asc"))
        .respond_with(ok("catalog_anime.json"))
        .expect(1)
        .mount(&server)
        .await;

    let filter = CatalogFilter {
        episodes: Bounds::at_least(12),
        sort: CatalogSort::EpisodeCount,
        order: SortOrder::Ascending,
        ..Default::default()
    };
    let page = client(&server)
        .catalog_filtered(Site::AnimeLib, &filter, 1)
        .await
        .unwrap();
    assert_eq!(page.items[0].site(), Some(Site::AnimeLib));
    // В `meta` ответа аниме нет `has_next_page`, но `links.next` есть.
    assert!(page.has_next);
}

#[tokio::test]
async fn site_id_five_goes_to_anime_endpoint() {
    let server = MockServer::start().await;
    mount(&server, "anime", ok("catalog_anime.json")).await;
    let filter = CatalogFilter {
        site_ids: vec![5],
        ..Default::default()
    };
    let page = client(&server)
        .catalog_filtered(Site::MangaLib, &filter, 1)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn target_filter_for_person_titles() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/manga"))
        .and(query_param("target_model", "people"))
        .and(query_param("target_id", "113727"))
        .respond_with(ok("catalog_manga.json"))
        .expect(1)
        .mount(&server)
        .await;
    let filter = CatalogFilter::of(TargetModel::People, 113727);
    assert_eq!(
        filter.target,
        Some(Target {
            model: TargetModel::People,
            id: 113727
        })
    );
    client(&server)
        .catalog_filtered(Site::MangaLib, &filter, 1)
        .await
        .unwrap();
}

#[tokio::test]
async fn random_order_cursor_travels_between_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/manga"))
        .and(query_param("sort_by", "random_order"))
        .and(query_param("page", "1"))
        .and(query_param_is_missing("seed"))
        .respond_with(ok("catalog_manga.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/manga"))
        .and(query_param("sort_by", "random_order"))
        .and(query_param("page", "2"))
        .and(query_param("seed", "6f722d27ae79acdce142486711099825"))
        .and(query_param("random_order", "0.617244741235508"))
        .respond_with(ok("catalog_manga.json"))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    let mut filter = CatalogFilter {
        sort: CatalogSort::Random,
        ..Default::default()
    };
    // Без курсора вторую страницу не отправляем вовсе.
    assert!(matches!(
        c.catalog_filtered(Site::MangaLib, &filter, 2).await,
        Err(Error::Other(_))
    ));
    let first = c.catalog_filtered(Site::MangaLib, &filter, 1).await.unwrap();
    assert_eq!(
        RandomCursor::of(&first),
        Some(RandomCursor {
            seed: "6f722d27ae79acdce142486711099825".into(),
            random_order: "0.617244741235508".into()
        })
    );
    filter.random = RandomCursor::of(&first);
    c.catalog_filtered(Site::MangaLib, &filter, 2).await.unwrap();
}

#[tokio::test]
async fn invalid_filter_never_reaches_the_server() {
    let server = MockServer::start().await;
    // Любой запрос к серверу провалит ожидание.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let c = client(&server);
    let by_chapters = CatalogFilter {
        sort: CatalogSort::ChapterCount,
        ..Default::default()
    };
    assert!(c.catalog_filtered(Site::AnimeLib, &by_chapters, 1).await.is_err());
    let novel_format = CatalogFilter {
        formats: vec![9],
        ..Default::default()
    };
    assert!(c.catalog_filtered(Site::MangaLib, &novel_format, 1).await.is_err());
}

#[tokio::test]
async fn catalog_constants_by_site() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/constants"))
        .and(query_param("fields[]", "genres"))
        .and(query_param("fields[]", "ageRestrictionAnime"))
        .and(query_param("fields[]", "scanlateStatus"))
        .respond_with(ok("catalog_constants.json"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server).catalog_constants().await.unwrap();

    assert_eq!(c.genres_for(1).len(), 1);
    assert_eq!(c.genres_for(5).len(), 2);
    assert!(c.genre(84).unwrap().is_blocked_in("ru"));
    assert_eq!(c.genre(84).unwrap().anilist_id.as_deref(), Some("1446"));
    assert_eq!(c.tags_for(4).len(), 1);
    assert!(c.tags_for(1).is_empty());
    // Тип «Неизвестный» без сайтов нигде не виден, у каждого сайта свои типы.
    assert_eq!(c.types.len(), 4);
    assert_eq!(c.types_for(1).len(), 1);
    assert_eq!(c.types_for(3)[0].label, "Япония");
    assert_eq!(c.types_for(5)[0].id, 16);
    // Нуль в site_ids статус общим не делает: у AnimeLib нет «Выпуск прекращён».
    assert_eq!(c.statuses_for(5).len(), 1);
    assert_eq!(c.statuses_for(1).len(), 2);
    assert_eq!(c.scanlate_statuses_for(5)[0].id, 6);
    assert_eq!(c.age_ratings_for(1)[0].label, "6+");
    assert_eq!(c.age_ratings_for(5)[0].label, "PG");
    let ranobe_formats = c.formats_for(3);
    assert_eq!(ranobe_formats[0].short_name.as_deref(), Some("LN"));
    assert!(!ranobe_formats[0].is_filterable());
    assert!(c.formats_for(1)[0].is_filterable());
    assert_eq!(c.content_marking[0].id, 1);
}

// --- ленты -----------------------------------------------------------------------------

#[tokio::test]
async fn home_page_all_blocks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/"))
        .and(header("Site-Id", "1"))
        .respond_with(ok("feeds_home.json"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let home = c.home(Site::MangaLib).await.unwrap();

    assert_eq!(home.popular.len(), 1);
    let item = &home.popular[0].items[0];
    assert_eq!((item.volume.as_deref(), item.number.as_str()), (Some("8"), "43.1"));
    assert!(!item.is_restricted());
    assert_eq!(home.latest_updates[0].items_count.as_ref().unwrap().uploaded, Some(37));
    assert_eq!(home.newest.len(), 1);
    assert!(home.newest[0].items.is_empty());
    assert!(home.hot_newest.is_empty());

    let groups: Vec<u32> = home.currently_views.groups.iter().map(|g| g.popularity).collect();
    assert_eq!(groups, vec![1, 2, 3]);
    assert_eq!(home.currently_views.groups[0].entries[0].views, 815);
    assert_eq!(home.currently_views.time, "day");

    assert_eq!(home.weekly_top_users[0].points_info.as_ref().unwrap().level, 11);
    assert_eq!(home.visited_news, vec!["308_0"]);
    assert_eq!(home.forum[0].category.as_ref().unwrap().name, "Баги и проблемы");
    assert_eq!(home.forum[0].owner.as_ref().unwrap().username, "user2");
    assert_eq!(home.reviews[0].text().plain_text(), "Текст отзыва");
    assert_eq!(home.reviews[0].rating.len(), 2);
    assert_eq!(home.reviews[0].related.as_ref().unwrap().id, 226802);
    assert_eq!(home.collections[0].items_count, 16);
    assert_eq!(
        home.slider[0].desktop.as_deref(),
        Some("/uploads/slider_items/35/d.jpg")
    );
    assert_eq!(
        c.upload_url("/uploads/slider_items/35/d.jpg"),
        format!("{}/uploads/slider_items/35/d.jpg", server.uri())
    );
}

#[tokio::test]
async fn latest_updates_of_anime_page_by_links_next() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/latest-updates"))
        .and(header("Site-Id", "5"))
        .and(query_param("page", "2"))
        .respond_with(ok("feeds_latest_anime.json"))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server).latest_updates(Site::AnimeLib, 2).await.unwrap();
    assert!(page.has_next);
    let episode = &page.items[0].items[0];
    assert!(episode.is_episode());
    assert_eq!(episode.season.as_deref(), Some("1"));
    assert_eq!(episode.players[0].team.as_ref().unwrap().name, "Команда 1");
    assert_eq!(page.items[0].items_count.as_ref().unwrap().total, Some(12));
}

#[tokio::test]
async fn user_updates_need_login() {
    let server = MockServer::start().await;
    mount(&server, "user-latest-updates", ResponseTemplate::new(401)).await;
    let result = client(&server).user_latest_updates(Site::MangaLib, 1).await;
    assert!(matches!(result, Err(Error::Unauthorized)));
}

#[tokio::test]
async fn top_views_both_modes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/media/top-views"))
        .and(query_param("time", "week"))
        .and(query_param_is_missing("popularity"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": fixture("feeds_home.json")["data"]["currently_views"].clone()})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/media/top-views"))
        .and(query_param("time", "day"))
        .and(query_param("popularity", "3"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": 206, "name": "One Piece", "slug_url": "206--one-piece", "views_day": 2716, "site": 1,
                      "rating": {"average": "9.72", "votes": 35894}}],
            "links": {"next": null},
            "meta": {"current_page": 2, "per_page": 56, "has_next_page": false}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    let groups = c.top_views(Site::MangaLib, TopTime::Week).await.unwrap();
    assert_eq!(groups.groups.len(), 3);
    let page = c
        .top_views_page(Site::MangaLib, TopTime::Day, TopGroup::Popular, 2)
        .await
        .unwrap();
    assert!(!page.has_next);
    assert_eq!(page.items[0].views, Some(2716));
    assert!(page.items[0].title.rating.as_ref().unwrap().votes > 0);
    // Группа аниме на сайте манги – ошибка клиента, а не пустой ответ сервера.
    assert!(
        c.top_views_page(Site::MangaLib, TopTime::Day, TopGroup::Ongoing, 1)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hot_newest_empty() {
    let server = MockServer::start().await;
    mount(
        &server,
        "media/hot-newest",
        ResponseTemplate::new(200).set_body_json(json!({"data": []})),
    )
    .await;
    let page = client(&server).hot_newest(Site::MangaLib, 1).await.unwrap();
    assert!(page.items.is_empty());
    assert!(!page.has_next);
}

#[tokio::test]
async fn news_list_and_article() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/news"))
        .and(query_param("type[]", "changelog"))
        .and(query_param("page", "1"))
        .respond_with(ok("feeds_news_list.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/news/308--versiia-204"))
        .respond_with(ok("feeds_news_article.json"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);

    let filter = NewsFilter {
        kinds: vec![NewsKind::Changelog],
        ..Default::default()
    };
    let page = c.news(Site::MangaLib, &filter, 1).await.unwrap();
    assert_eq!(page.total, Some(187));
    assert_eq!(page.last_page, Some(10));
    assert!(page.has_next);
    assert_eq!(page.items[0].visited_key(), "308_0");
    // `contentVersion` приходит и числом.
    assert_eq!(page.items[1].visited_key(), "305_3");
    assert_eq!(page.items[1].version_name.as_deref(), Some("2.0.4"));

    let article = c.news_article(Site::MangaLib, &page.items[0].slug_url).await.unwrap();
    assert_eq!(article.item.title, "Версия 2.0.4");
    assert_eq!(article.author.as_ref().unwrap().username, "user1");
    assert_eq!(article.attachments.len(), 1);
    assert!(article.content.plain_text().contains("Читалка переписана"));
    // Картинка в тексте ссылалась на вложение по имени, теперь это адрес на хосте API.
    let Block::Images(pictures) = &article.content.blocks[1] else {
        panic!("{:?}", article.content.blocks);
    };
    assert_eq!(
        pictures[0].url,
        format!(
            "{}/uploads/news/308/ec5f957d-a5e7-4210-a4b1-e0f8192db049.gif",
            server.uri()
        )
    );
    assert_eq!(pictures[0].width, Some(800));
    assert!(article.content_raw["content"].is_array());
}

// --- поиск -----------------------------------------------------------------------------

#[tokio::test]
async fn people_search_with_cursor_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/people"))
        .and(query_param("q", "ito"))
        .and(query_param("sort_by", "subscribes_count"))
        .and(query_param("sort_type", "desc"))
        .and(query_param("site_ids[]", "1"))
        .respond_with(ok("search_people.json"))
        .expect(1)
        .mount(&server)
        .await;
    let query = EntityQuery {
        text: "ito".into(),
        sort: Some(EntitySort::Subscribers),
        site_ids: vec![1],
        ..Default::default()
    };
    let page = client(&server)
        .search_entities(EntityKind::People, &query, 1)
        .await
        .unwrap();
    assert!(page.has_next);
    assert_eq!(page.total, None);
    let person = &page.items[0];
    assert_eq!(person.display_name(), "Дзюндзи Ито");
    assert_eq!(person.titles_on(1), 48);
    assert_eq!(person.titles_on(5), 3);
    assert_eq!(person.subscribers(), Some(38666));
    assert_eq!(person.alt_names(), vec!["Ито Дзюндзи", "Junji Ito"]);
    assert_eq!(person.subscription.as_ref().unwrap().source_id, Some(113727));
}

#[tokio::test]
async fn characters_with_full_pagination_and_document() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/character"))
        .and(query_param("limit", "3"))
        .respond_with(ok("search_characters.json"))
        .expect(1)
        .mount(&server)
        .await;
    let query = EntityQuery {
        limit: Some(3),
        ..Default::default()
    };
    let page = client(&server)
        .search_entities(EntityKind::Character, &query, 1)
        .await
        .unwrap();
    assert_eq!(page.total, Some(107806));
    assert_eq!(page.last_page, Some(35936));
    assert!(page.has_next);
    let character = &page.items[0];
    // Слаг пустой, но slug_url всё равно годится для пути.
    assert_eq!(character.slug_url, "109850--");
    assert_eq!(character.description().plain_text(), "Приехала в Корею");
    assert!(character.sites_with_titles().is_empty());
}

#[tokio::test]
async fn short_query_is_refused_before_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let result = client(&server)
        .search_entities(EntityKind::Team, &EntityQuery::search("а"), 1)
        .await;
    assert!(matches!(result, Err(Error::Other(_))));
}

#[tokio::test]
async fn entity_detail_by_slug() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/teams/1343--team1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "id": 1343, "slug": "team1", "slug_url": "1343--team1", "model": "team", "name": "Команда 1",
            "vk": "https://vk.com/team1", "website": null,
            "description": "Переводим мангу",
            "stats": [{"value": 160, "formated": "160", "short": "160", "label": "Тайтлов", "tag": "titles"},
                      {"value": 5, "formated": "5", "short": "5", "label": "Глав", "tag": "chapters"}],
            "titles_count_details": {"1": 160}
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let team = client(&server).entity(EntityKind::Team, "1343--team1").await.unwrap();
    assert_eq!(team.vk.as_deref(), Some("https://vk.com/team1"));
    assert_eq!(team.stat("chapters"), Some(5));
    assert_eq!(team.description().plain_text(), "Переводим мангу");
}

#[tokio::test]
async fn users_points_come_without_meta() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .and(query_param("filter", "points"))
        .respond_with(ok("search_users_points.json"))
        .expect(1)
        .mount(&server)
        .await;
    let query = UserQuery {
        filter: UserFilter::Points,
        ..Default::default()
    };
    let page = client(&server).users(&query, 1).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(!page.has_next);
    assert_eq!(page.items[0].points_info.as_ref().unwrap().top, Some(1));
    assert!(page.items[0].is_premium());
    assert!(page.items[0].avatar.as_ref().unwrap().is_placeholder());
    assert!(!page.items[1].avatar.as_ref().unwrap().is_placeholder());
}

#[tokio::test]
async fn user_search_sends_text_and_sort() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .and(query_param("q", "user1"))
        .and(query_param("sort_by", "id"))
        .and(query_param("sort_type", "asc"))
        .and(query_param_is_missing("filter"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [], "links": {"next": null}, "meta": {"current_page": 1, "per_page": 48}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let query = UserQuery {
        order: SortOrder::Ascending,
        ..UserQuery::search("user1")
    };
    let page = client(&server).users(&query, 1).await.unwrap();
    assert!(page.items.is_empty());
}

// --- живые проверки --------------------------------------------------------------------

fn live() -> Client {
    Client::new(ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn live_catalog_filters() {
    let c = live();
    let filter = CatalogFilter {
        genres: vec![34],
        sort: CatalogSort::Rating,
        years: Bounds::at_least(2010),
        ..Default::default()
    };
    let page = c.catalog_filtered(Site::MangaLib, &filter, 1).await.unwrap();
    assert!(!page.items.is_empty());
    assert!(page.items[0].rating.as_ref().unwrap().votes >= 50);
    assert!(page.has_next);

    let anime = c
        .catalog_filtered(Site::AnimeLib, &CatalogFilter::search("bleach"), 1)
        .await
        .unwrap();
    assert!(!anime.items.is_empty());
    // У аниме `has_next_page` нет, а один экран результатов поиска страницу не заполняет.
    assert!(!anime.has_next);

    let ito = c
        .catalog_filtered(Site::MangaLib, &CatalogFilter::of(TargetModel::People, 113727), 1)
        .await
        .unwrap();
    assert!(!ito.items.is_empty());

    let random = CatalogFilter {
        sort: CatalogSort::Random,
        ..Default::default()
    };
    let first = c.catalog_filtered(Site::MangaLib, &random, 1).await.unwrap();
    let second = CatalogFilter {
        random: RandomCursor::of(&first),
        ..random
    };
    assert!(
        !c.catalog_filtered(Site::MangaLib, &second, 2)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
#[ignore]
async fn live_constants_and_feeds() {
    let c = live();
    let constants = c.catalog_constants().await.unwrap();
    assert!(constants.genres.len() > 40);
    assert!(constants.tags.len() > 300);
    assert!(!constants.types_for(5).is_empty());
    assert!(!constants.age_ratings_for(5).is_empty());

    for site in [Site::MangaLib, Site::AnimeLib] {
        let home = c.home(site).await.unwrap();
        assert!(!home.popular.is_empty(), "{site:?}");
        assert!(!home.currently_views.groups.is_empty());
        let latest = c.latest_updates(site, 1).await.unwrap();
        assert!(!latest.items.is_empty());
        assert!(latest.items.iter().any(|t| !t.items.is_empty()));
    }
    let top = c
        .top_views_page(Site::AnimeLib, TopTime::Day, TopGroup::Ongoing, 1)
        .await
        .unwrap();
    assert!(!top.items.is_empty());
}

#[tokio::test]
#[ignore]
async fn live_news_and_search() {
    let c = live();
    let news = c.news(Site::MangaLib, &NewsFilter::default(), 1).await.unwrap();
    assert!(news.total.unwrap_or(0) > 20);
    let article = c.news_article(Site::MangaLib, &news.items[0].slug_url).await.unwrap();
    assert!(!article.content.is_empty());

    let people = c
        .search_entities(EntityKind::People, &EntityQuery::search("junji"), 1)
        .await
        .unwrap();
    assert!(!people.items.is_empty());
    let franchises = c
        .search_entities(EntityKind::Franchise, &EntityQuery::search("маг"), 1)
        .await
        .unwrap();
    assert!(!franchises.items.is_empty());
    let moderators = c
        .users(
            &UserQuery {
                filter: UserFilter::Moderators,
                ..Default::default()
            },
            1,
        )
        .await
        .unwrap();
    assert!(moderators.items.iter().any(|u| !u.roles.is_empty()));
}
