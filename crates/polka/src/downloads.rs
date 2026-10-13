//! Загрузчик: берёт задания из очереди в базе по одному и качает их на диск.
//!
//! Очередь живёт в базе, поэтому переживает перезапуск: прерванное задание снова
//! встаёт в очередь, а уже скачанные страницы и куски видео не качаются повторно.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, TryStreamExt};
use libsocial::models::PlayerKind;
use libsocial::{Client, Site};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, broadcast};

use crate::db::{Db, DownloadRow, DownloadState, TitleRow};
use crate::library::{self, CHAPTER_MARKER, Job, Library};

/// Сообщение для интерфейса: у задания поменялось состояние или прогресс.
#[derive(Debug, Clone)]
pub struct DownloadEvent {
    pub site: Site,
    pub slug: String,
    pub item: String,
    pub state: DownloadState,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

/// Задание, которое сейчас выполняется, и флажок для его остановки.
struct Running {
    site: Site,
    slug: String,
    item: String,
    stop: Arc<AtomicBool>,
    /// Что сделать после остановки: поставить на паузу или удалить.
    remove: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct Downloader {
    client: Client,
    db: Db,
    library: Library,
    concurrency: usize,
    image_server: String,
    wake: Arc<Notify>,
    running: Arc<Mutex<Option<Running>>>,
    events: broadcast::Sender<DownloadEvent>,
}

impl Downloader {
    /// Создаёт загрузчик и запускает его рабочую задачу в текущем рантайме tokio.
    pub fn start(client: Client, db: Db, library: Library, concurrency: usize, image_server: String) -> Downloader {
        let (events, _) = broadcast::channel(256);
        let downloader = Downloader {
            client,
            db,
            library,
            concurrency: concurrency.clamp(1, 8),
            image_server,
            wake: Arc::new(Notify::new()),
            running: Arc::new(Mutex::new(None)),
            events,
        };
        if let Err(e) = downloader.db.requeue_interrupted() {
            eprintln!("не удалось вернуть прерванные загрузки в очередь: {e:#}");
        }
        let worker = downloader.clone();
        tokio::spawn(async move { worker.run().await });
        downloader
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.events.subscribe()
    }

    /// Ставит задание в очередь. Карточку тайтла сохраняет в базу и рядом с главами,
    /// чтобы библиотека открывалась без сети. Возвращает `false`, если задание уже
    /// скачано или уже ждёт в очереди.
    pub fn enqueue(&self, title: &TitleRow, label: &str, job: Job) -> Result<bool> {
        self.db.upsert_title(title)?;
        if let Some(details) = &title.details
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(details)
        {
            self.library.write_title_meta(title.site, &title.slug, &value)?;
        }
        let path = self.library.job_path(title.site, &title.slug, &job);
        let row = DownloadRow {
            site: title.site,
            slug: title.slug.clone(),
            item: job.item_key(),
            label: label.to_string(),
            sort: job.sort(),
            state: DownloadState::Queued,
            done: 0,
            total: 0,
            error: None,
            path: path.to_string_lossy().into_owned(),
            job: serde_json::to_string(&job)?,
        };
        let added = self.db.enqueue(&row)?;
        if added {
            self.emit(&row.site, &row.slug, &row.item, DownloadState::Queued, 0, 0, None);
            self.wake.notify_one();
        }
        Ok(added)
    }

    /// Ставит на паузу. Выполняющееся задание останавливается после текущей страницы.
    pub fn pause(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        if self.signal_running(site, slug, item, false) {
            return Ok(());
        }
        self.db.set_state(site, slug, item, DownloadState::Paused, None)?;
        self.emit(&site, slug, item, DownloadState::Paused, 0, 0, None);
        Ok(())
    }

    pub fn resume(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        self.db.set_state(site, slug, item, DownloadState::Queued, None)?;
        self.emit(&site, slug, item, DownloadState::Queued, 0, 0, None);
        self.wake.notify_one();
        Ok(())
    }

    /// Убирает задание из очереди и стирает его файлы.
    pub fn remove(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        if self.signal_running(site, slug, item, true) {
            return Ok(());
        }
        self.remove_now(site, slug, item)
    }

    fn remove_now(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        let rows = self.db.downloads_for(site, slug)?;
        if let Some(row) = rows.iter().find(|r| r.item == item) {
            remove_path(Path::new(&row.path))?;
        }
        self.db.remove_download(site, slug, item)?;
        self.emit(&site, slug, item, DownloadState::Failed, 0, 0, Some("удалено".into()));
        Ok(())
    }

    /// Если задание сейчас выполняется, просит его остановиться.
    fn signal_running(&self, site: Site, slug: &str, item: &str, remove: bool) -> bool {
        let running = self.running.lock().expect("running отравлен");
        match running.as_ref() {
            Some(r) if r.site == site && r.slug == slug && r.item == item => {
                r.remove.store(remove, Ordering::SeqCst);
                r.stop.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        site: &Site,
        slug: &str,
        item: &str,
        state: DownloadState,
        done: u64,
        total: u64,
        error: Option<String>,
    ) {
        let _ = self.events.send(DownloadEvent {
            site: *site,
            slug: slug.to_string(),
            item: item.to_string(),
            state,
            done,
            total,
            error,
        });
    }

    async fn run(self) {
        loop {
            let next = match self.db.next_queued() {
                Ok(next) => next,
                Err(e) => {
                    eprintln!("очередь загрузок недоступна: {e:#}");
                    None
                }
            };
            match next {
                Some(row) => self.process(row).await,
                None => self.wake.notified().await,
            }
        }
    }

    async fn process(&self, row: DownloadRow) {
        let stop = Arc::new(AtomicBool::new(false));
        let remove = Arc::new(AtomicBool::new(false));
        *self.running.lock().expect("running отравлен") = Some(Running {
            site: row.site,
            slug: row.slug.clone(),
            item: row.item.clone(),
            stop: stop.clone(),
            remove: remove.clone(),
        });
        let _ = self
            .db
            .set_state(row.site, &row.slug, &row.item, DownloadState::Running, None);
        self.emit(
            &row.site,
            &row.slug,
            &row.item,
            DownloadState::Running,
            row.done,
            row.total,
            None,
        );
        self.ensure_cover(row.site, &row.slug).await;

        let result = match serde_json::from_str::<Job>(&row.job) {
            Ok(job @ Job::Chapter { .. }) => self.chapter(&row, &job, &stop).await,
            Ok(job @ Job::Episode { .. }) => self.episode(&row, &job, &stop).await,
            Err(e) => Err(anyhow!("испорченное задание: {e}")),
        };
        *self.running.lock().expect("running отравлен") = None;

        let (state, error) = match result {
            Ok(Outcome::Done) => (DownloadState::Done, None),
            Ok(Outcome::Stopped) if remove.load(Ordering::SeqCst) => {
                if let Err(e) = self.remove_now(row.site, &row.slug, &row.item) {
                    eprintln!("не удалось удалить загрузку: {e:#}");
                }
                return;
            }
            Ok(Outcome::Stopped) => (DownloadState::Paused, None),
            Err(e) => (DownloadState::Failed, Some(format!("{e:#}"))),
        };
        let _ = self
            .db
            .set_state(row.site, &row.slug, &row.item, state, error.as_deref());
        let (done, total) = self
            .db
            .downloads_for(row.site, &row.slug)
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.item == row.item))
            .map(|r| (r.done, r.total))
            .unwrap_or((0, 0));
        self.emit(&row.site, &row.slug, &row.item, state, done, total, error);
    }

    fn report(&self, row: &DownloadRow, done: u64, total: u64) {
        let _ = self
            .db
            .set_download_progress(row.site, &row.slug, &row.item, done, total);
        self.emit(
            &row.site,
            &row.slug,
            &row.item,
            DownloadState::Running,
            done,
            total,
            None,
        );
    }

    async fn chapter(&self, row: &DownloadRow, job: &Job, stop: &Arc<AtomicBool>) -> Result<Outcome> {
        let Job::Chapter {
            volume,
            number,
            branch_id,
            ..
        } = job
        else {
            unreachable!("вызывается только для глав");
        };
        let dir = PathBuf::from(&row.path);
        if library::chapter_complete(&dir) {
            return Ok(Outcome::Done);
        }
        tokio::fs::create_dir_all(&dir).await?;
        let content = self
            .client
            .chapter_pages_by(row.site, &row.slug, volume, number, *branch_id)
            .await
            .context("список страниц")?;
        if content.pages.is_empty() {
            return Err(anyhow!("в главе нет страниц"));
        }
        let total = content.pages.len() as u64;
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        self.report(row, 0, total);

        // Задачи страниц владеют всем, что им нужно: так их можно гонять параллельно
        // внутри фоновой задачи без споров о временах жизни.
        let jobs: Vec<_> = content
            .pages
            .into_iter()
            .enumerate()
            .map(|(i, page)| {
                let target = dir.join(library::page_file_name(i, page.extension()));
                let done = done.clone();
                let this = self.clone();
                let row = row.clone();
                let stop = stop.clone();
                async move {
                    if stop.load(Ordering::SeqCst) {
                        return Ok::<bool, anyhow::Error>(false);
                    }
                    if !target.is_file() {
                        let bytes = this
                            .client
                            .fetch_page(row.site, &page, &this.image_server)
                            .await
                            .with_context(|| format!("страница {}", i + 1))?;
                        write_file(&target, &bytes).await?;
                    }
                    let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                    this.report(&row, n, total);
                    Ok(true)
                }
            })
            .collect();
        let finished: Vec<bool> = futures::stream::iter(jobs)
            .buffer_unordered(self.concurrency)
            .try_collect()
            .await?;
        if stop.load(Ordering::SeqCst) || finished.iter().any(|ok| !ok) {
            return Ok(Outcome::Stopped);
        }
        // Отметка пишется последней: по ней читалка понимает, что глава целая.
        let marker = serde_json::json!({
            "volume": volume,
            "number": number,
            "name": content.name,
            "branch_id": branch_id,
            "pages": total,
        });
        write_file(&dir.join(CHAPTER_MARKER), &serde_json::to_vec_pretty(&marker)?).await?;
        Ok(Outcome::Done)
    }

    async fn episode(&self, row: &DownloadRow, job: &Job, stop: &AtomicBool) -> Result<Outcome> {
        let Job::Episode {
            episode_id,
            player_id,
            max_quality,
            ..
        } = job
        else {
            unreachable!("вызывается только для эпизодов");
        };
        let target = PathBuf::from(&row.path);
        if target.is_file() {
            return Ok(Outcome::Done);
        }
        let details = self.client.episode(*episode_id).await.context("данные эпизода")?;
        let player = details
            .players
            .iter()
            .find(|p| p.id == *player_id && p.kind() == PlayerKind::Animelib)
            .ok_or_else(|| anyhow!("этой озвучки больше нет на сервере, выберите другую"))?;
        let video = self
            .client
            .resolve_video(player, *max_quality, None, true)
            .await
            .context("ссылка на видео")?;

        if let Some(dir) = target.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        for sub in &video.subtitles {
            let path = target.with_extension(sub.format.trim_start_matches('.'));
            if !path.is_file()
                && let Ok(bytes) = self.client.fetch_bytes(Site::AnimeLib, &sub.url).await
            {
                write_file(&path, &bytes).await?;
            }
        }

        let part = part_path(&target);
        let mut last_error = None;
        for url in std::iter::once(&video.url).chain(&video.fallbacks) {
            match self.stream_to(row, url, &part, stop).await {
                Ok(Outcome::Done) => {
                    tokio::fs::rename(&part, &target).await?;
                    return Ok(Outcome::Done);
                }
                Ok(Outcome::Stopped) => return Ok(Outcome::Stopped),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("видео не скачалось")))
    }

    /// Качает файл в `.part`, продолжая с того места, где остановились.
    async fn stream_to(&self, row: &DownloadRow, url: &str, part: &Path, stop: &AtomicBool) -> Result<Outcome> {
        let have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
        let response = match self.client.fetch_stream(Site::AnimeLib, url, have).await {
            Ok(r) => r,
            // Сервер не умеет докачивать: начинаем заново.
            Err(_) if have > 0 => {
                tokio::fs::remove_file(part).await.ok();
                self.client.fetch_stream(Site::AnimeLib, url, 0).await?
            }
            Err(e) => return Err(e.into()),
        };
        let have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
        let total = response.content_length().map(|n| n + have).unwrap_or(0);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .await?;
        let mut done = have;
        let mut reported = done;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            if stop.load(Ordering::SeqCst) {
                file.flush().await?;
                return Ok(Outcome::Stopped);
            }
            let chunk = chunk.context("обрыв загрузки видео")?;
            file.write_all(&chunk).await?;
            done += chunk.len() as u64;
            if done - reported > 2 * 1024 * 1024 {
                reported = done;
                self.report(row, done, total);
            }
        }
        file.flush().await?;
        if total > 0 && done < total {
            return Err(anyhow!("видео оборвалось на {} из {} МБ", done >> 20, total >> 20));
        }
        self.report(row, done, total.max(done));
        Ok(Outcome::Done)
    }

    /// Обложка рядом с главами, чтобы библиотека показывала её без сети.
    async fn ensure_cover(&self, site: Site, slug: &str) {
        let path = self.library.cover_path(site, slug);
        if path.is_file() {
            return;
        }
        let Ok(Some(title)) = self.db.title(site, slug) else {
            return;
        };
        let Some(url) = title.cover_url else { return };
        if let Ok(bytes) = self.client.fetch_bytes(site, &url).await {
            let _ = write_file(&path, &bytes).await;
        }
    }
}

enum Outcome {
    Done,
    Stopped,
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// Пишет файл через `.part`, чтобы оборванная запись не выглядела готовой страницей.
async fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let part = part_path(path);
    tokio::fs::write(&part, bytes).await?;
    tokio::fs::rename(&part, path).await?;
    Ok(())
}

/// Стирает каталог главы или файл эпизода вместе с недокачанным куском и субтитрами.
fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        for candidate in [path.to_path_buf(), part_path(path)] {
            match std::fs::remove_file(&candidate) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        if let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) {
            for ext in ["vtt", "ass", "srt"] {
                let _ = std::fs::remove_file(dir.join(format!("{stem}.{ext}")));
            }
        }
    }
    Ok(())
}
