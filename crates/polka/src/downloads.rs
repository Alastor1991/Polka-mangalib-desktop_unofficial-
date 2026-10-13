//! Загрузчик: берёт задания из очереди в базе по одному и качает их на диск.
//!
//! Очередь живёт в базе и переживает перезапуск: прерванное задание снова встаёт
//! в очередь, скачанные страницы и куски видео повторно не качаются. Качает только
//! один процесс: он держит блокировку файла, остальные лишь ставят задания в очередь.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, TryStreamExt};
use libsocial::models::{PlayerKind, image_format};
use libsocial::{Client, Site};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, broadcast};

use crate::db::{Db, DownloadRow, DownloadState, TitleRow};
use crate::library::{self, CHAPTER_MARKER, Job, Library};

/// Сколько ждать очередной кусок видео, прежде чем считать соединение зависшим.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
/// Как часто заглядывать в очередь без сигнала: задания мог добавить другой процесс.
const POLL: Duration = Duration::from_secs(10);
/// Пауза между заданиями, чтобы не идти к серверу сплошным потоком.
const BETWEEN_JOBS: Duration = Duration::from_millis(300);

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
    /// Задание удалено вместе с файлами.
    pub removed: bool,
}

/// Выполняющееся задание и флажки для его остановки.
struct Running {
    site: Site,
    slug: String,
    item: String,
    stop: Arc<AtomicBool>,
    /// После остановки удалить, а не поставить на паузу.
    remove: Arc<AtomicBool>,
}

impl Running {
    fn is(&self, site: Site, slug: &str, item: &str) -> bool {
        self.site == site && self.slug == slug && self.item == item
    }
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
    /// Тайтлы, обложку которых уже не удалось скачать в этом запуске.
    cover_failed: Arc<Mutex<HashSet<(Site, String)>>>,
    /// Блокировка «качаю я». Пока файл открыт, второй процесс не качает.
    lock: Arc<Mutex<Option<std::fs::File>>>,
}

impl Downloader {
    /// Создаёт загрузчик и, если этот процесс первый, запускает рабочую задачу
    /// в текущем рантайме tokio.
    pub fn start(
        client: Client,
        db: Db,
        library: Library,
        concurrency: usize,
        image_server: String,
        lock_path: &Path,
    ) -> Downloader {
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
            cover_failed: Arc::new(Mutex::new(HashSet::new())),
            lock: Arc::new(Mutex::new(None)),
        };
        if downloader.try_activate(lock_path) {
            downloader.spawn_worker();
        } else {
            // Качает другой процесс. Когда он закончит, очередь возьмём мы.
            let waiter = downloader.clone();
            let path = lock_path.to_path_buf();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(POLL).await;
                    if waiter.try_activate(&path) {
                        waiter.spawn_worker();
                        return;
                    }
                }
            });
        }
        downloader
    }

    fn try_activate(&self, lock_path: &Path) -> bool {
        match take_lock(lock_path) {
            Some(file) => {
                *self.lock.lock().expect("lock отравлен") = Some(file);
                true
            }
            None => false,
        }
    }

    fn spawn_worker(&self) {
        if let Err(e) = self.db.requeue_interrupted() {
            eprintln!("не удалось вернуть прерванные загрузки в очередь: {e:#}");
        }
        let worker = self.clone();
        tokio::spawn(async move { worker.run().await });
    }

    /// Этот процесс качает сам. Иначе задания выполнит процесс, держащий блокировку.
    pub fn is_active(&self) -> bool {
        self.lock.lock().expect("lock отравлен").is_some()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.events.subscribe()
    }

    pub fn event_sender(&self) -> broadcast::Sender<DownloadEvent> {
        self.events.clone()
    }

    /// Ставит задание в очередь. Карточку тайтла сохраняет в базу и рядом с главами,
    /// чтобы библиотека открывалась без сети. Возвращает `false`, если задание уже
    /// скачано, качается или ждёт в очереди.
    pub fn enqueue(&self, title: &TitleRow, label: &str, job: Job) -> Result<bool> {
        self.db.upsert_title(title)?;
        let meta = self.library.title_dir(title.site, &title.slug).join("title.json");
        if !meta.is_file()
            && let Some(details) = &title.details
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(details)
        {
            self.library.write_title_meta(title.site, &title.slug, &value)?;
        }
        let path = self.library.job_path(title.site, &title.slug, &job);
        let item = job.item_key();
        // Повторная постановка с другой озвучкой или качеством: старый недокачанный
        // файл к новому заданию не относится.
        if let Some(old) = self.db.download(title.site, &title.slug, &item)?
            && matches!(old.state, DownloadState::Failed | DownloadState::Paused)
            && old.path != path.to_string_lossy()
        {
            remove_path(Path::new(&old.path))?;
        }
        let row = DownloadRow {
            site: title.site,
            slug: title.slug.clone(),
            item,
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
            self.emit_row(row.site, &row.slug, &row.item, false);
            self.wake.notify_one();
        }
        Ok(added)
    }

    /// Ставит пачку заданий одного тайтла одной транзакцией. Возвращает, сколько
    /// добавлено. Вызывать вне потока интерфейса: запись идёт синхронно.
    pub fn enqueue_many(&self, title: &TitleRow, jobs: Vec<(String, Job)>) -> Result<usize> {
        self.db.upsert_title(title)?;
        let meta = self.library.title_dir(title.site, &title.slug).join("title.json");
        if !meta.is_file()
            && let Some(details) = &title.details
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(details)
        {
            self.library.write_title_meta(title.site, &title.slug, &value)?;
        }
        let rows: Vec<DownloadRow> = jobs
            .into_iter()
            .map(|(label, job)| {
                Ok(DownloadRow {
                    site: title.site,
                    slug: title.slug.clone(),
                    item: job.item_key(),
                    label,
                    sort: job.sort(),
                    state: DownloadState::Queued,
                    done: 0,
                    total: 0,
                    error: None,
                    path: self
                        .library
                        .job_path(title.site, &title.slug, &job)
                        .to_string_lossy()
                        .into_owned(),
                    job: serde_json::to_string(&job)?,
                })
            })
            .collect::<Result<_>>()?;
        let added = self.db.enqueue_many(&rows)?;
        if added > 0 {
            // Одним сигналом: интерфейс перечитает очередь сам.
            let _ = self.events.send(DownloadEvent {
                site: title.site,
                slug: title.slug.clone(),
                item: String::new(),
                state: DownloadState::Queued,
                done: 0,
                total: 0,
                error: None,
                removed: false,
            });
            self.wake.notify_one();
        }
        Ok(added)
    }

    /// Ставит на паузу. Выполняющееся задание останавливается после текущей страницы.
    pub fn pause(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        let running = self.running.lock().expect("running отравлен");
        if let Some(r) = running.as_ref().filter(|r| r.is(site, slug, item)) {
            r.remove.store(false, Ordering::SeqCst);
            r.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        if self
            .db
            .transition(site, slug, item, &[DownloadState::Queued], DownloadState::Paused)?
        {
            self.emit_row(site, slug, item, false);
        }
        Ok(())
    }

    pub fn resume(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        let from = [DownloadState::Paused, DownloadState::Failed];
        if self.db.transition(site, slug, item, &from, DownloadState::Queued)? {
            self.emit_row(site, slug, item, false);
            self.wake.notify_one();
        }
        Ok(())
    }

    /// Убирает задание из очереди и стирает его файлы. Работающее задание сначала
    /// останавливается, а стирает его сам рабочий цикл.
    pub fn remove(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        // Замок держим до конца: рабочий цикл не захватит и не завершит задание,
        // пока мы его стираем.
        let running = self.running.lock().expect("running отравлен");
        if let Some(r) = running.as_ref().filter(|r| r.is(site, slug, item)) {
            r.remove.store(true, Ordering::SeqCst);
            r.stop.store(true, Ordering::SeqCst);
            return Ok(());
        }
        self.remove_now(site, slug, item)
    }

    fn remove_now(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        if let Some(row) = self.db.download(site, slug, item)? {
            remove_path(Path::new(&row.path))?;
        }
        self.db.remove_download(site, slug, item)?;
        self.emit(DownloadEvent {
            site,
            slug: slug.to_string(),
            item: item.to_string(),
            state: DownloadState::Failed,
            done: 0,
            total: 0,
            error: None,
            removed: true,
        });
        Ok(())
    }

    fn emit(&self, event: DownloadEvent) {
        let _ = self.events.send(event);
    }

    /// Шлёт событие с тем, что сейчас записано в базе.
    fn emit_row(&self, site: Site, slug: &str, item: &str, removed: bool) {
        if let Ok(Some(row)) = self.db.download(site, slug, item) {
            self.emit(DownloadEvent {
                site,
                slug: slug.to_string(),
                item: item.to_string(),
                state: row.state,
                done: row.done,
                total: row.total,
                error: row.error,
                removed,
            });
        }
    }

    async fn run(self) {
        let mut failures: u32 = 0;
        loop {
            let next = match self.db.next_queued() {
                Ok(next) => next,
                Err(e) => {
                    eprintln!("очередь загрузок недоступна: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let Some(row) = next else {
                let _ = tokio::time::timeout(POLL, self.wake.notified()).await;
                continue;
            };
            let (site, slug, item) = (row.site, row.slug.clone(), row.item.clone());
            // Задание выполняется в своей задаче: паника из-за странных данных
            // сервера не должна убить весь загрузчик.
            let worker = self.clone();
            let state = match tokio::spawn(async move { worker.process(row).await }).await {
                Ok(state) => state,
                Err(e) => {
                    *self.running.lock().expect("running отравлен") = None;
                    let msg = format!("внутренняя ошибка загрузчика: {e}");
                    let _ = self.db.set_state(site, &slug, &item, DownloadState::Failed, Some(&msg));
                    self.emit_row(site, &slug, &item, false);
                    Some(DownloadState::Failed)
                }
            };
            match state {
                Some(DownloadState::Failed) => {
                    failures += 1;
                    // Сервер лежит или нас не пускают: не долбим его следующими заданиями.
                    if failures >= 2 {
                        let pause = Duration::from_secs((5u64 << (failures - 2).min(6)).min(300));
                        let _ = tokio::time::timeout(pause, self.wake.notified()).await;
                    }
                }
                Some(DownloadState::Done) => failures = 0,
                _ => {}
            }
            tokio::time::sleep(BETWEEN_JOBS).await;
        }
    }

    /// Выполняет задание. Возвращает итоговое состояние, `None` если задание не
    /// досталось этому процессу или удалено.
    async fn process(&self, row: DownloadRow) -> Option<DownloadState> {
        let stop = Arc::new(AtomicBool::new(false));
        let remove = Arc::new(AtomicBool::new(false));
        {
            let mut running = self.running.lock().expect("running отравлен");
            // Захват под замком: пауза или удаление между чтением очереди и захватом
            // либо уже изменили строку, либо увидят задание выполняющимся.
            if !self.db.claim(row.site, &row.slug, &row.item).unwrap_or(false) {
                return None;
            }
            *running = Some(Running {
                site: row.site,
                slug: row.slug.clone(),
                item: row.item.clone(),
                stop: stop.clone(),
                remove: remove.clone(),
            });
        }
        self.emit_row(row.site, &row.slug, &row.item, false);
        self.ensure_cover(row.site, &row.slug).await;

        let result = match serde_json::from_str::<Job>(&row.job) {
            Ok(job @ Job::Chapter { .. }) => self.chapter(&row, &job, &stop).await,
            Ok(job @ Job::Episode { .. }) => self.episode(&row, &job, &stop).await,
            Err(e) => Err(anyhow!("испорченное задание: {e}")),
        };

        // Решение по итогу и снятие отметки «выполняется» под одним замком, чтобы
        // запрос на удаление не проскочил между ними.
        let mut running = self.running.lock().expect("running отравлен");
        let removing = remove.load(Ordering::SeqCst);
        *running = None;
        if removing {
            if let Err(e) = self.remove_now(row.site, &row.slug, &row.item) {
                eprintln!("не удалось удалить загрузку: {e:#}");
            }
            return None;
        }
        let (state, error) = match result {
            Ok(Outcome::Done) => (DownloadState::Done, None),
            Ok(Outcome::Stopped) => (DownloadState::Paused, None),
            Err(e) => (DownloadState::Failed, Some(format!("{e:#}"))),
        };
        let _ = self
            .db
            .set_state(row.site, &row.slug, &row.item, state, error.as_deref());
        drop(running);
        self.emit_row(row.site, &row.slug, &row.item, false);
        Some(state)
    }

    fn report(&self, row: &DownloadRow, done: u64, total: u64) {
        let _ = self
            .db
            .set_download_progress(row.site, &row.slug, &row.item, done, total);
        self.emit(DownloadEvent {
            site: row.site,
            slug: row.slug.clone(),
            item: row.item.clone(),
            state: DownloadState::Running,
            done,
            total,
            error: None,
            removed: false,
        });
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
                let dir = dir.clone();
                let done = done.clone();
                let this = self.clone();
                let row = row.clone();
                let stop = stop.clone();
                async move {
                    if stop.load(Ordering::SeqCst) {
                        return Ok::<bool, anyhow::Error>(false);
                    }
                    if library::find_page(&dir, i).is_none() {
                        let bytes = this
                            .client
                            .fetch_page(row.site, &page, &this.image_server)
                            .await
                            .with_context(|| format!("страница {}", i + 1))?;
                        // Расширение по содержимому: сервер сжатия отдаёт JPEG под именем .png.
                        let ext = image_format(&bytes)
                            .ok_or_else(|| anyhow!("страница {}: сервер вернул не картинку", i + 1))?;
                        write_file(&dir.join(library::page_file_name(i, ext)), &bytes).await?;
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
            let path = target.with_extension(subtitle_ext(&sub.format));
            if !path.is_file()
                && let Ok(bytes) = self.client.fetch_bytes(Site::AnimeLib, &sub.url).await
            {
                write_file(&path, &bytes).await?;
            }
        }

        // Недокачанный кусок годится, только если он от того же файла: та же озвучка
        // и то же качество. Сервер может быть другим, путь файла в API на всех один.
        let part = part_path(&target);
        let identity = PartIdentity {
            file: video.href.clone(),
            quality: video.quality,
        };
        let sidecar = sidecar_path(&part);
        let saved: Option<PartIdentity> = tokio::fs::read(&sidecar)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if saved.as_ref() != Some(&identity) {
            let _ = tokio::fs::remove_file(&part).await;
        }
        write_file(&sidecar, &serde_json::to_vec(&identity)?).await?;

        let mut last_error = None;
        for url in std::iter::once(&video.url).chain(&video.fallbacks) {
            match self.stream_to(row, url, &part, stop).await {
                Ok(Outcome::Done) => {
                    tokio::fs::rename(&part, &target).await?;
                    let _ = tokio::fs::remove_file(&sidecar).await;
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
            // Кусок за концом файла: всё уже скачано, процесс прервали перед переименованием.
            Err(libsocial::Error::RangeNotSatisfiable) => return Ok(Outcome::Done),
            // Сервер не умеет докачивать: только в этом случае начинаем заново.
            Err(libsocial::Error::RangeUnsupported) => {
                tokio::fs::remove_file(part).await.ok();
                self.client.fetch_stream(Site::AnimeLib, url, 0).await?
            }
            // Остальные ошибки временные: недокачанное сохраняем до следующей попытки.
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
        loop {
            if stop.load(Ordering::SeqCst) {
                file.flush().await?;
                return Ok(Outcome::Stopped);
            }
            let chunk = match tokio::time::timeout(CHUNK_TIMEOUT, body.next()).await {
                Ok(Some(chunk)) => chunk.context("обрыв загрузки видео")?,
                Ok(None) => break,
                Err(_) => {
                    file.flush().await?;
                    return Err(anyhow!("сервер перестал отдавать видео, продолжу позже"));
                }
            };
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
        let key = (site, slug.to_string());
        if path.is_file() || self.cover_failed.lock().expect("cover_failed отравлен").contains(&key) {
            return;
        }
        let Ok(Some(title)) = self.db.title(site, slug) else {
            return;
        };
        let Some(url) = title.cover_url else { return };
        match self.client.fetch_bytes(site, &url).await {
            Ok(bytes) if image_format(&bytes).is_some() => {
                let _ = write_file(&path, &bytes).await;
            }
            _ => {
                self.cover_failed.lock().expect("cover_failed отравлен").insert(key);
            }
        }
    }
}

enum Outcome {
    Done,
    Stopped,
}

/// Что именно лежит в `.part`: путь файла на сервере и качество.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PartIdentity {
    file: String,
    quality: u64,
}

/// Расширение субтитров из ответа сервера: только буквы и цифры, иначе vtt.
fn subtitle_ext(format: &str) -> &str {
    let f = format.trim().trim_start_matches('.');
    if !f.is_empty() && f.len() <= 5 && f.chars().all(|c| c.is_ascii_alphanumeric()) {
        f
    } else {
        "vtt"
    }
}

fn take_lock(path: &Path) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

fn sidecar_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".json");
    part.with_file_name(name)
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

/// Стирает каталог главы или файл эпизода вместе с недокачанным куском, его
/// описанием и субтитрами. Имя эпизода уникально (сезон, серия, озвучка), поэтому
/// все файлы `<имя>.*` рядом принадлежат ему.
fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
        return Ok(());
    }
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) else {
        return Ok(());
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let prefix = format!("{stem}.");
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| n.starts_with(&prefix)) && entry.path().is_file() {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_extensions() {
        assert_eq!(subtitle_ext("vtt"), "vtt");
        assert_eq!(subtitle_ext(".ass"), "ass");
        assert_eq!(subtitle_ext("../x"), "vtt");
        assert_eq!(subtitle_ext("a/b"), "vtt");
        assert_eq!(subtitle_ext(""), "vtt");
    }

    #[test]
    fn remove_episode_files_only() {
        let dir = std::env::temp_dir().join(format!("polka-rm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "s1-e1-p5.mp4.part",
            "s1-e1-p5.mp4.part.json",
            "s1-e1-p5.vtt",
            "s1-e10-p5.mp4",
            "cover.jpg",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        remove_path(&dir.join("s1-e1-p5.mp4")).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["cover.jpg", "s1-e10-p5.mp4"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_lock_holder() {
        let path = std::env::temp_dir().join(format!("polka-lock-{}", std::process::id()));
        let first = take_lock(&path);
        assert!(first.is_some());
        assert!(take_lock(&path).is_none());
        drop(first);
        assert!(take_lock(&path).is_some());
        let _ = std::fs::remove_file(&path);
    }
}
