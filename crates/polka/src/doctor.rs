//! `polka doctor`: проверка, что приложение достаёт до всех частей сервиса.

use anyhow::Result;
use libsocial::Site;
use libsocial::catalog::CatalogFilter;
use libsocial::models::PlayerKind;

use polka_core::config::Config;
use polka_core::session;

/// Тайтл с длинной историей и собственным плеером, на нём проверяется видео.
const ANIME_PROBE: &[&str] = &["16133--jujutsu-kaisen-anime", "14421--kimetsu-no-yaiba-anime"];

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let (client, store) = session::connect(&config).await?;

    match client
        .catalog_filtered(Site::MangaLib, &CatalogFilter::search("берсерк"), 1)
        .await
    {
        Ok(_) => println!("✓ API: {}", client.api_base()),
        Err(e) => {
            println!("✗ API недоступен ни через одно зеркало: {e}");
            return Ok(());
        }
    }
    session::remember_mirror(&config, &client);

    if client.is_logged_in().await {
        match client.me().await {
            Ok(me) => println!("✓ вход: {} ({})", me.username, store.describe()),
            Err(e) => println!("✗ вход сохранён, но не работает: {e}"),
        }
    } else {
        println!("• вход не выполнен: 18+ разделы и плеер AnimeLib недоступны (polka login)");
    }

    for site in [Site::SlashLib, Site::HentaiLib, Site::AnimeLib] {
        match client.catalog_filtered(site, &CatalogFilter::default(), 1).await {
            Ok(p) if !p.items.is_empty() => println!("✓ каталог {}", site.name()),
            Ok(_) => println!("• каталог {} пуст (возможно, нужен вход)", site.name()),
            Err(e) => println!("✗ каталог {}: {e}", site.name()),
        }
    }

    match image_check(&client, &config).await {
        Ok(size) => println!("✓ картинки: страница главы скачана, {} КБ", size / 1024),
        Err(e) => println!("✗ картинки: {e}"),
    }

    match anime_check(&client, &config).await {
        Ok(msg) => println!("{msg}"),
        Err(e) => println!("✗ аниме: {e}"),
    }
    Ok(())
}

async fn image_check(client: &libsocial::Client, config: &Config) -> libsocial::Result<usize> {
    let slug = "7965--chainsaw-man";
    let chapters = client.chapters(Site::MangaLib, slug).await?;
    let chapter = chapters
        .iter()
        .find(|c| c.branches.iter().all(|b| b.is_open()))
        .ok_or(libsocial::Error::NotFound)?;
    let pages = client
        .chapter_pages(Site::MangaLib, slug, chapter, chapter.branches[0].branch_id)
        .await?;
    let page = pages.pages.first().ok_or(libsocial::Error::NotFound)?;
    Ok(client
        .fetch_page(Site::MangaLib, page, &config.image_server)
        .await?
        .len())
}

async fn anime_check(client: &libsocial::Client, config: &Config) -> libsocial::Result<String> {
    for slug in ANIME_PROBE {
        let episodes = client.episodes(slug).await?;
        let Some(first) = episodes.first() else { continue };
        let episode = client.episode(first.id).await?;
        let own = episode
            .players
            .iter()
            .filter(|p| p.kind() == PlayerKind::Animelib)
            .count();
        let kodik = episode.players.iter().filter(|p| p.kind() == PlayerKind::Kodik).count();
        if own == 0 {
            continue;
        }
        let player = episode
            .players
            .iter()
            .find(|p| p.kind() == PlayerKind::Animelib)
            .expect("посчитан выше");
        let video = client.resolve_video(player, config.max_quality, None, true).await?;
        return Ok(format!(
            "✓ аниме: плеер AnimeLib есть ({own} вариантов, Kodik {kodik}), {}p отвечает, субтитров {}",
            video.quality,
            video.subtitles.len()
        ));
    }
    Ok("• аниме: собственного плеера AnimeLib в проверочных тайтлах нет, только Kodik".into())
}

pub async fn dev_get(args: &[String]) -> Result<()> {
    let [site, path, rest @ ..] = args else {
        anyhow::bail!("использование: polka dev-get <mangalib|slashlib|hentailib|animelib> <путь> [ключ=значение…]");
    };
    let site = Site::from_key(site).ok_or_else(|| anyhow::anyhow!("неизвестный сайт {site}"))?;
    let query: Vec<(String, String)> = rest
        .iter()
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let config = Config::load()?;
    let (client, _) = session::connect(&config).await?;
    let value = client.raw_get(site, path, &query).await?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
