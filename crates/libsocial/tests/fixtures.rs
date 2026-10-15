//! Разбор настоящих ответов API, сохранённых в `tests/fixtures`.

use libsocial::Site;
use libsocial::listing::Page;
use libsocial::models::*;
use serde::de::DeserializeOwned;

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn data<T: DeserializeOwned>(name: &str) -> T {
    serde_json::from_value(fixture(name)["data"].clone()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn catalog_pages() {
    for name in ["search.json", "popular.json", "asearch.json", "latest.json"] {
        let page: Page<TitleShort> = serde_json::from_value(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!page.items.is_empty(), "{name}");
        assert!(page.items.iter().all(|t| !t.slug_url.is_empty()));
    }
    let popular: Page<TitleShort> = serde_json::from_value(fixture("popular.json")).unwrap();
    assert!(popular.has_next);
    let anime: Page<TitleShort> = serde_json::from_value(fixture("asearch.json")).unwrap();
    assert_eq!(anime.items[0].site(), Some(Site::AnimeLib));
    assert_eq!(anime.items[0].model, "anime");
    assert!(anime.items[0].shiki_rate.is_some_and(|r| r > 0.0));
}

#[test]
fn catalog_card_extras() {
    let page: Page<TitleShort> = serde_json::from_value(fixture("catalog_manga.json")).unwrap();
    let first = &page.items[0];
    assert_eq!((first.model.as_str(), first.slug.as_str()), ("manga", "chainsaw-man"));
    let rating = first.rating.as_ref().unwrap();
    assert_eq!(rating.average_formatted.as_deref(), Some("9.4"));
    assert_eq!(rating.votes_formatted.as_deref(), Some("93.8 K"));
    let bookmark = first.user_bookmark.as_deref().unwrap();
    assert_eq!((bookmark.status, bookmark.item_id), (1, Some(5)));
    assert_eq!(bookmark.meta.item_number, Some(4));

    let popular: Page<TitleShort> = serde_json::from_value(fixture("popular.json")).unwrap();
    assert_eq!(popular.items[0].content_marking[0].label, "Содержит нецензурную брань");
    assert!(popular.items.iter().all(|t| t.user_bookmark.is_none()));

    // Закладка, которая не разобралась, не уносит с собой карточку.
    let odd: TitleShort = serde_json::from_value(serde_json::json!({
        "id": 1, "slug_url": "1--x", "userBookmark": {"id": "x"}, "shiki_rate": "8.5"
    }))
    .unwrap();
    assert!(odd.user_bookmark.is_none());
    assert_eq!(odd.shiki_rate, Some(8.5));
}

#[test]
fn title_details() {
    let t: TitleDetails = data("details_full.json");
    assert_eq!(t.short.display_name(), "Человек-бензопила");
    assert!(t.short.rating.as_ref().unwrap().average > 9.0);
    assert!(!t.genres.is_empty());
    let summary = t.summary_text();
    assert!(summary.starts_with('«'), "{summary}");
    assert!(!summary.contains("\"type\""));
    let _: TitleDetails = data("details.json");
}

/// Карточку тайтла ядро хранит в базе и читает обратно без сети: всё, что в ней
/// есть, должно пережить круг записи и чтения.
#[test]
fn title_details_roundtrip() {
    let mut t: TitleDetails = data("details_full.json");
    let catalog: libsocial::Page<TitleShort> = serde_json::from_value(fixture("catalog_manga.json")).unwrap();
    t.short.user_bookmark = catalog.items[0].user_bookmark.clone();
    t.short.content_marking = catalog.items[0].content_marking.clone();
    t.short.shiki_rate = Some(8.5);

    let back: TitleDetails = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    assert_eq!(back.short.slug_url, t.short.slug_url);
    assert_eq!(back.short.shiki_rate, Some(8.5));
    assert_eq!(back.short.content_marking.len(), t.short.content_marking.len());
    let (a, b) = (back.short.user_bookmark.unwrap(), t.short.user_bookmark.unwrap());
    assert_eq!(
        (a.status, a.item_id, a.meta.item_number),
        (b.status, b.item_id, b.meta.item_number)
    );
    assert_eq!(back.genres.len(), t.genres.len());
}

#[test]
fn chapters_and_branches() {
    let simple: Vec<Chapter> = data("chapters.json");
    assert_eq!(simple[0].title(), "Том 1 Глава 1 – Сингл");

    let multi: Vec<Chapter> = data("chapters_branches.json");
    assert!(multi.iter().any(|c| c.branches.len() > 1));
    let branches: Vec<Branch> = data("branches.json");
    assert!(!branches.is_empty());

    let paid: Vec<Chapter> = data("chapters_paid.json");
    let locked: Vec<_> = paid.iter().filter(|c| !c.branches[0].is_open()).collect();
    assert!(!locked.is_empty());
    let rv = locked[0].branches[0].restricted_view.as_ref().unwrap();
    assert!(rv.expired_at.is_some());
    assert!(paid.iter().filter(|c| c.branches[0].is_open()).count() > locked.len());
}

#[test]
fn pages_and_servers() {
    let pages: ChapterPages = data("pages.json");
    assert!(!pages.pages.is_empty());
    assert_eq!(pages.pages[0].extension(), "jpg");
    assert_eq!(pages.pages[1].extension(), "png");

    let constants: Constants = data("constants.json");
    let servers = constants.image_servers_for(Site::MangaLib, "compress");
    assert_eq!(servers[0].id, "compress");
    let url = servers[0].join(&pages.pages[0].url);
    assert_eq!(
        url,
        "https://img3.cdnlibs.org/manga/berserk-memorial/chapters/1766671/0_CK0f.jpg"
    );
    assert!(!constants.image_servers_for(Site::SlashLib, "compress").is_empty());
}

#[test]
fn anime() {
    let episodes: Vec<Episode> = data("episodes.json");
    assert_eq!(episodes[0].title(), "1 серия – Черный мечник");
    let ep: EpisodeDetails = data("episode.json");
    assert!(ep.players.iter().all(|p| p.kind() == PlayerKind::Kodik));
    assert!(ep.players[0].embed_url().unwrap().starts_with("https://kodik"));
    let constants: Constants = data("aconst.json");
    assert_eq!(constants.video_servers.len(), 3);
}

#[test]
fn video_quality_pick() {
    let v: Video = serde_json::from_value(serde_json::json!({
        "id": 1,
        "quality": [
            {"href": "/a/360.mp4", "quality": 360, "bitrate": 1},
            {"href": "/a/720.mp4", "quality": 720, "bitrate": 2},
            {"href": "/a/1080.mp4", "quality": "1080", "bitrate": 3}
        ]
    }))
    .unwrap();
    assert_eq!(v.pick(1080).unwrap().quality, 1080);
    assert_eq!(v.pick(900).unwrap().quality, 720);
    assert_eq!(v.pick(240).unwrap().quality, 360);
}

#[test]
fn animelib_player() {
    let ep: EpisodeDetails = data("episode_animelib.json");
    let own: Vec<_> = ep.players.iter().filter(|p| p.kind() == PlayerKind::Animelib).collect();
    assert!(!own.is_empty());
    let player = own[0];
    let video = player.video.as_ref().unwrap();
    let best = video.pick(1080).unwrap();
    assert_eq!(best.quality, 1080);
    assert!(best.href.starts_with("/uploads/"));

    let constants: Constants = data("aconst.json");
    let server = constants
        .video_servers
        .iter()
        .find(|s| Some(&s.id) == player.video_domain.as_ref())
        .unwrap();
    let url = server.join(&best.href);
    assert!(url.starts_with("https://") && url.ends_with("_1080.mp4"), "{url}");
    assert!(!url.contains(".аs//"), "{url}");
}

/// Закладки: синтетические ответы с устройством настоящих, без данных аккаунта.
#[test]
fn bookmarks() {
    use libsocial::{Bookmark, BookmarkFolder};

    let folders: Vec<BookmarkFolder> = data("bookmark_folders.json");
    assert!(
        folders
            .iter()
            .any(|f| f.id == 1 && f.is_for(Site::MangaLib) && f.is_for(Site::HentaiLib))
    );
    assert!(
        folders
            .iter()
            .any(|f| f.id == 21 && f.is_for(Site::AnimeLib) && !f.is_for(Site::MangaLib))
    );

    let one: Bookmark = data("bookmark.json");
    assert_eq!((one.status, one.item_id), (21, Some(41924)));
    assert_eq!(one.progress.as_deref(), Some("12:34"));
    let player = one.player().unwrap();
    assert_eq!(
        (player.player.as_str(), player.team, player.translation_type),
        ("Animelib", 32412, 2)
    );

    let list = fixture("bookmarks.json")["data"].clone();
    let first: Bookmark = serde_json::from_value(list[0].clone()).unwrap();
    assert_eq!(first.media.as_ref().unwrap().slug_url, "7965--chainsaw-man");
    assert_eq!(first.item.as_ref().unwrap().number, "2");
    assert_eq!(first.meta.item_number, Some(2));
    assert_eq!(first.updated_secs(), Some(1_725_944_452));
    assert!(first.player().is_none());
    let empty: Bookmark = serde_json::from_value(list[1].clone()).unwrap();
    assert_eq!((empty.item_id, empty.progress), (None, None));
}

#[test]
fn view_marks() {
    let pages: ChapterPages = data("pages.json");
    assert_eq!(pages.manga_id, Some(109483));
    assert!(!pages.is_viewed);
    let episode: EpisodeDetails = data("episode_animelib.json");
    assert!(episode.players.iter().all(|p| !p.is_viewed));
    let animelib = episode
        .players
        .iter()
        .find(|p| p.kind() == PlayerKind::Animelib)
        .unwrap();
    assert!(animelib.choice().is_some());
}
