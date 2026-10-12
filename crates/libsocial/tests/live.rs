//! Живые проверки против настоящего API. Запуск:
//!
//! ```sh
//! POLKA_INTERFACE=enp4s0 cargo test -p libsocial --test live -- --ignored --test-threads=1
//! ```
//!
//! `POLKA_INTERFACE` нужен, если системный VPN выводит трафик в страну, откуда сервис недоступен.

use libsocial::{CatalogQuery, Client, ClientConfig, Site};

fn client() -> Client {
    let config = ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        ..Default::default()
    };
    Client::new(config).unwrap()
}

#[tokio::test]
#[ignore]
async fn manga_flow() {
    let c = client();
    let found = c.search(Site::MangaLib, "берсерк", 1).await.unwrap();
    assert!(!found.data.is_empty());

    let title = c.title(Site::MangaLib, "7965--chainsaw-man").await.unwrap();
    assert!(!title.summary_text().is_empty());

    let chapters = c.chapters(Site::MangaLib, "7965--chainsaw-man").await.unwrap();
    let chapter = chapters
        .iter()
        .find(|c| c.branches.iter().all(|b| b.is_open()))
        .unwrap();
    let branch = chapter.branches[0].branch_id;
    let pages = c
        .chapter_pages(Site::MangaLib, "7965--chainsaw-man", chapter, branch)
        .await
        .unwrap();
    assert!(!pages.pages.is_empty());

    let image = c.fetch_page(Site::MangaLib, &pages.pages[0], "compress").await.unwrap();
    assert!(
        image.len() > 10_000,
        "картинка подозрительно маленькая: {}",
        image.len()
    );
}

#[tokio::test]
#[ignore]
async fn other_sites_catalog() {
    let c = client();
    for site in [Site::SlashLib, Site::AnimeLib] {
        let page = c
            .catalog(site, &CatalogQuery::default())
            .await
            .unwrap_or_else(|e| panic!("{site:?}: {e}"));
        assert!(!page.data.is_empty(), "{site:?}");
    }
}

#[tokio::test]
#[ignore]
async fn anime_flow() {
    let c = client();
    let episodes = c.episodes("24--kenpuu-denki-berserk-anime").await.unwrap();
    assert!(!episodes.is_empty());
    let ep = c.episode(episodes[0].id).await.unwrap();
    assert!(!ep.players.is_empty());
    assert_eq!(c.video_servers().await.unwrap().len(), 3);
}
