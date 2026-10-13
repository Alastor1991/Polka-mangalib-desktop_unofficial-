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
    let title = c.title(Site::AnimeLib, "24--kenpuu-denki-berserk-anime").await.unwrap();
    assert!(!title.summary_text().is_empty());
    let episodes = c.episodes("24--kenpuu-denki-berserk-anime").await.unwrap();
    assert!(!episodes.is_empty());
    let ep = c.episode(episodes[0].id).await.unwrap();
    assert!(!ep.players.is_empty());
    assert_eq!(c.video_servers().await.unwrap().len(), 3);
}

#[tokio::test]
#[ignore]
async fn mirror_failover() {
    let config = ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        api_mirrors: vec![
            "https://api.invalid/api".into(),
            "https://api2.mangalib.me/api".into(),
            "https://api.cdnlibs.org/api".into(),
        ],
        ..Default::default()
    };
    let c = Client::new(config).unwrap();
    let page = c.search(Site::MangaLib, "берсерк", 1).await.unwrap();
    assert!(!page.data.is_empty());
    println!("рабочее зеркало: {}", c.api_base());
    assert_ne!(c.api_base(), "https://api.invalid/api");
}

#[tokio::test]
#[ignore]
async fn comments_flow() {
    use libsocial::comments::{CommentSort, Target};

    let c = client();
    let page = c.comments(Target::Manga(7965), CommentSort::Popular, 1).await.unwrap();
    assert!(!page.roots.is_empty());
    assert!(page.roots.iter().all(|r| r.is_root()));
    assert!(page.roots.iter().any(|r| !r.text().plain_text().is_empty()));
    assert!(page.replies.iter().all(|r| r.parent_comment.is_some()));

    // Комментарии к первой странице первой главы: с ответами.
    let chapter = Target::Chapter {
        chapter_id: 223269,
        page: Some(1),
    };
    let on_page = c.comments(chapter, CommentSort::Newest, 1).await.unwrap();
    assert!(on_page.roots.iter().all(|r| r.post_page == Some(1)));

    let root = &page.roots[0];
    let thread = c
        .comment_thread(Target::Manga(7965), root.id, CommentSort::Newest)
        .await
        .unwrap();
    assert_eq!(thread.roots[0].id, root.id);

    let sticky = c.sticky_comments(Target::Manga(7965)).await.unwrap();
    let _ = sticky;
    let reasons = c.report_types().await.unwrap();
    assert!(reasons.iter().any(|r| !r.label.is_empty()));
    // Стикеры сервер отдаёт только вошедшему.
    match c.sticker_packs().await {
        Ok(packs) => assert!(packs.iter().any(|p| !p.stickers.is_empty())),
        Err(e) => assert!(matches!(e, libsocial::Error::Unauthorized), "{e}"),
    }
}
