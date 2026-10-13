//! Команды без интерфейса: скачивание из терминала.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use libsocial::models::{Chapter, PlayerKind, TitleDetails};
use libsocial::{Client, Site};

use crate::config::Config;
use crate::db::{Db, DownloadState, TitleRow};
use crate::downloads::Downloader;
use crate::library::{Job, Library};
use crate::session;

/// `polka download <сайт> <slug> [от-до]`: ставит главы или серии в очередь и ждёт.
pub async fn download(args: &[String]) -> Result<()> {
    let [site, slug, rest @ ..] = args else {
        bail!("использование: polka download <mangalib|slashlib|hentailib|animelib> <slug> [от-до]");
    };
    let site = Site::from_key(site).with_context(|| format!("неизвестный сайт {site}"))?;
    let range = rest.first().map(|r| parse_range(r)).transpose()?;

    let config = Config::load()?;
    let (client, _) = session::connect(&config).await?;
    let db = Db::open(&config.db_path())?;
    let library = Library::new(config.library_dir());
    let downloader = Downloader::start(
        client.clone(),
        db.clone(),
        library,
        config.download_concurrency,
        config.image_server.clone(),
        &config.lock_path(),
    );
    if !downloader.is_active() {
        println!("Загрузки уже идут в открытом приложении Полка: задания встанут в его очередь.");
    }
    let mut events = downloader.subscribe();

    let details = client.title(site, slug).await.context("карточка тайтла")?;
    let title = title_row(site, &details);
    println!("{} ({})", details.short.display_name(), site.name());

    let jobs = if site.is_anime() {
        anime_jobs(&client, slug, range, config.max_quality).await?
    } else {
        manga_jobs(&client, site, slug, &details, range).await?
    };
    if jobs.is_empty() {
        println!("Нечего скачивать в этом диапазоне.");
        return Ok(());
    }
    let mut waiting = 0;
    for (label, job) in &jobs {
        if downloader.enqueue(&title, label, job.clone())? {
            waiting += 1;
        }
    }
    println!("В очереди: {waiting}, уже скачано или ждёт: {}", jobs.len() - waiting);

    while has_pending(&db, site, slug)? {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
            Ok(Ok(e)) if e.slug == *slug => match e.state {
                DownloadState::Running if e.total > 0 => {
                    eprint!(
                        "\r{:<14} {:>6}/{:<6}",
                        e.item,
                        human(e.done, site),
                        human(e.total, site)
                    );
                }
                DownloadState::Done => eprintln!("\r{:<14} готово{:20}", e.item, ""),
                DownloadState::Failed if !e.removed => {
                    eprintln!("\r{:<14} ошибка: {}", e.item, e.error.unwrap_or_default())
                }
                _ => {}
            },
            _ => {}
        }
    }
    let rows = db.downloads_for(site, slug)?;
    let failed = rows.iter().filter(|r| r.state == DownloadState::Failed).count();
    let done = rows.iter().filter(|r| r.state == DownloadState::Done).count();
    println!(
        "\nСкачано: {done}, с ошибкой: {failed}. Папка: {}",
        config.library_dir().display()
    );
    Ok(())
}

fn has_pending(db: &Db, site: Site, slug: &str) -> Result<bool> {
    Ok(db
        .downloads_for(site, slug)?
        .iter()
        .any(|r| matches!(r.state, DownloadState::Queued | DownloadState::Running)))
}

/// Для глав прогресс в страницах, для видео в байтах.
fn human(n: u64, site: Site) -> String {
    if site.is_anime() {
        format!("{}М", n >> 20)
    } else {
        n.to_string()
    }
}

fn parse_range(s: &str) -> Result<(f64, f64)> {
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let a: f64 = a.trim().parse().with_context(|| format!("не число: {a}"))?;
    let b: f64 = b.trim().parse().with_context(|| format!("не число: {b}"))?;
    Ok((a.min(b), a.max(b)))
}

pub fn title_row(site: Site, details: &TitleDetails) -> TitleRow {
    TitleRow {
        site,
        slug: details.short.slug_url.clone(),
        title_id: details.short.id,
        name: details.short.display_name().to_string(),
        cover_url: details.short.cover.best().map(str::to_string),
        details: serde_json::to_string(details).ok(),
        in_library: false,
    }
}

/// Ветка перевода для главы: выбранная на сайте по умолчанию, если она есть у главы.
pub fn pick_branch(chapter: &Chapter, preferred: Option<u64>) -> Option<u64> {
    chapter
        .branches
        .iter()
        .find(|b| preferred.is_some() && b.branch_id == preferred)
        .or(chapter.branches.first())
        .and_then(|b| b.branch_id)
}

async fn manga_jobs(
    client: &Client,
    site: Site,
    slug: &str,
    details: &TitleDetails,
    range: Option<(f64, f64)>,
) -> Result<Vec<(String, Job)>> {
    let chapters = client.chapters(site, slug).await.context("список глав")?;
    let multi = chapters.iter().any(|c| c.branches.len() > 1);
    let preferred = if multi {
        client
            .branches(site, details.short.id)
            .await
            .ok()
            .and_then(|b| b.first().map(|b| b.id))
    } else {
        None
    };
    let mut jobs = Vec::new();
    let mut locked = 0;
    for chapter in &chapters {
        let n = chapter.number_f32() as f64;
        if range.is_some_and(|(a, b)| n < a || n > b) {
            continue;
        }
        let branch = pick_branch(chapter, preferred);
        if !chapter.branch(branch).is_some_and(|b| b.is_open()) {
            locked += 1;
            continue;
        }
        jobs.push((chapter.title(), Job::chapter(chapter, branch)));
    }
    if locked > 0 {
        println!("Закрытых глав (ранний доступ или платные) пропущено: {locked}");
    }
    Ok(jobs)
}

/// Для аниме берётся одна команда на весь сериал: та, у которой больше всего серий
/// в собственном плеере AnimeLib.
async fn anime_jobs(
    client: &Client,
    slug: &str,
    range: Option<(f64, f64)>,
    max_quality: u64,
) -> Result<Vec<(String, Job)>> {
    if !client.is_logged_in().await {
        bail!("видео AnimeLib отдаётся только после входа: polka login");
    }
    let episodes = client.episodes(slug).await.context("список серий")?;
    let mut picked = Vec::new();
    for ep in &episodes {
        let n: f64 = ep.number.parse().unwrap_or(0.0);
        if range.is_some_and(|(a, b)| n < a || n > b) {
            continue;
        }
        let details = client.episode(ep.id).await?;
        picked.push((ep.clone(), details));
    }
    let mut teams: HashMap<u64, usize> = HashMap::new();
    for (_, d) in &picked {
        for p in d.players.iter().filter(|p| p.kind() == PlayerKind::Animelib) {
            *teams.entry(p.team.as_ref().map(|t| t.id).unwrap_or(0)).or_default() += 1;
        }
    }
    let team = teams.into_iter().max_by_key(|(_, n)| *n).map(|(t, _)| t);
    let mut jobs = Vec::new();
    for (ep, d) in &picked {
        let own: Vec<_> = d.players.iter().filter(|p| p.kind() == PlayerKind::Animelib).collect();
        let player = own
            .iter()
            .find(|p| p.team.as_ref().map(|t| t.id) == team)
            .or(own.first());
        match player {
            Some(p) => jobs.push((
                format!("{} ({})", ep.title(), p.team_name()),
                Job::episode(ep, p, max_quality),
            )),
            None => println!("{}: только Kodik, скачать нельзя", ep.title()),
        }
    }
    Ok(jobs)
}
