//! Живые проверки против настоящего API. Запуск:
//!
//! ```sh
//! POLKA_INTERFACE=enp4s0 cargo test -p libsocial --test live -- --ignored --test-threads=1
//! ```
//!
//! `POLKA_INTERFACE` нужен, если системный VPN выводит трафик в страну, откуда сервис недоступен.

use libsocial::catalog::CatalogFilter;
use libsocial::{Client, ClientConfig, Site};

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
    let found = c
        .catalog_filtered(Site::MangaLib, &CatalogFilter::search("берсерк"), 1)
        .await
        .unwrap();
    assert!(!found.items.is_empty());

    let title = c.title(Site::MangaLib, "7965--chainsaw-man").await.unwrap();
    assert!(!title.summary_text().is_empty());
    // Расширенные поля карточки: сервер их принимает и присылает.
    assert!(title.views.as_ref().is_some_and(|v| v.total > 0));
    assert!(!title.publisher.is_empty());
    assert!(title.metadata.is_some());
    assert!(title.items_count.as_ref().is_some_and(|i| i.uploaded > 0));

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
            .catalog_filtered(site, &CatalogFilter::default(), 1)
            .await
            .unwrap_or_else(|e| panic!("{site:?}: {e}"));
        assert!(!page.items.is_empty(), "{site:?}");
        // Поля карточки, которые разбираются без входа.
        let t = &page.items[0];
        assert!(!t.model.is_empty() && !t.slug.is_empty(), "{site:?}");
        assert!(
            t.rating.as_ref().is_some_and(|r| r.average_formatted.is_some()),
            "{site:?}"
        );
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
    let page = c
        .catalog_filtered(Site::MangaLib, &CatalogFilter::search("берсерк"), 1)
        .await
        .unwrap();
    assert!(!page.items.is_empty());
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

#[tokio::test]
#[ignore]
async fn title_extras() {
    use libsocial::media::{CharacterRole, ReviewFilter, TeamSort};
    use libsocial::search::EntityKind;

    let c = client();
    let slug = "7965--chainsaw-man";
    assert!(!c.covers(Site::MangaLib, slug).await.unwrap().is_empty());
    let stats = c.title_stats(Site::MangaLib, slug).await.unwrap();
    assert!(stats.rating.count > 0 && stats.rating.stats.len() == 10);
    let relations = c.relations(Site::MangaLib, slug).await.unwrap();
    assert!(relations.iter().any(|r| r.related_type.is_some()));
    let similar = c.similar(Site::MangaLib, slug).await.unwrap();
    assert!(!similar.is_empty() && !similar[0].similar.is_empty());
    let chars = c
        .title_characters(Site::MangaLib, 7965, Some(CharacterRole::Main), 1)
        .await
        .unwrap();
    assert!(!chars.items.is_empty());

    let title = c.title(Site::MangaLib, slug).await.unwrap();
    let author = &title.authors[0];
    // Страница человека гостю закрыта: сервер отвечает 404.
    match c.entity(EntityKind::People, author.slug_url.as_deref().unwrap()).await {
        Ok(person) => assert!(person.titles_on(Site::MangaLib.id()) > 0),
        Err(e) => assert!(matches!(e, libsocial::Error::NotFound), "{e}"),
    }

    let teams = c.teams("", TeamSort::Titles, 1).await.unwrap();
    assert!(teams.has_next);
    let team = &teams.items[0];
    // Карточка команды гостю закрыта, как и страница человека.
    match c.team(team.slug_url.as_deref().unwrap()).await {
        Ok(card) => assert!(!card.stats.is_empty()),
        Err(e) => assert!(matches!(e, libsocial::Error::NotFound), "{e}"),
    }
    assert!(!c.team_members(team.id).await.unwrap().is_empty());
    assert!(!c.team_chapters(team.id, 1).await.unwrap().items.is_empty());

    let reviews = c
        .reviews(&ReviewFilter {
            title: Some((Site::MangaLib, 7965)),
            page: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!reviews.items.is_empty());
    let one = c.review(reviews.items[0].id).await.unwrap();
    assert!(!one.text().plain_text().is_empty());

    let voices = c
        .manga_voices(Site::MangaLib, "6192--kusuriya-no-hitorigoto", 1)
        .await
        .unwrap();
    assert!(!voices.items.is_empty());
    if let Some(v) = voices.items.first() {
        assert!(c.manga_voice_url(v, 720).await.unwrap().is_some());
    }
}
