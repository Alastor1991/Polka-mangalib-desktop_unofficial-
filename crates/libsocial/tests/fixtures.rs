//! Разбор настоящих ответов API, сохранённых в `tests/fixtures`.

use libsocial::Site;
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
        let page: Paged<TitleShort> = serde_json::from_value(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!page.data.is_empty(), "{name}");
        assert!(page.data.iter().all(|t| !t.slug_url.is_empty()));
    }
    let popular: Paged<TitleShort> = serde_json::from_value(fixture("popular.json")).unwrap();
    assert!(popular.meta.has_next_page);
    let anime: Paged<TitleShort> = serde_json::from_value(fixture("asearch.json")).unwrap();
    assert_eq!(anime.data[0].site(), Some(Site::AnimeLib));
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
