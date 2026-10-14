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
        refuse_ranobe(title.site)?;
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
        refuse_ranobe(title.site)?;
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
            let verdict = match tokio::spawn(async move { worker.process(row).await }).await {
                Ok(verdict) => verdict,
                Err(e) => {
                    *self.running.lock().expect("running отравлен") = None;
                    let msg = format!("внутренняя ошибка загрузчика: {e}");
                    let _ = self.db.set_state(site, &slug, &item, DownloadState::Failed, Some(&msg));
                    self.emit_row(site, &slug, &item, false);
                    Verdict::Finished(DownloadState::Failed)
                }
            };
            match verdict {
                Verdict::Finished(DownloadState::Failed) => {
                    failures += 1;
                    // Сервер лежит или нас не пускают: не долбим его следующими заданиями.
                    if failures >= 2 {
                        let pause = Duration::from_secs((5u64 << (failures - 2).min(6)).min(300));
                        let _ = tokio::time::timeout(pause, self.wake.notified()).await;
                    }
                }
                Verdict::Finished(DownloadState::Done) => failures = 0,
                // Сервис попросил паузу. Новые задания её не отменяют, поэтому
                // сигнал очереди здесь не слушаем.
                Verdict::Wait(wait) => tokio::time::sleep(wait).await,
                _ => {}
            }
            tokio::time::sleep(BETWEEN_JOBS).await;
        }
    }

    /// Выполняет задание и говорит рабочему циклу, чем оно кончилось.
    async fn process(&self, row: DownloadRow) -> Verdict {
        let stop = Arc::new(AtomicBool::new(false));
        let remove = Arc::new(AtomicBool::new(false));
        {
            let mut running = self.running.lock().expect("running отравлен");
            // Захват под замком: пауза или удаление между чтением очереди и захватом
            // либо уже изменили строку, либо увидят задание выполняющимся.
            if !self.db.claim(row.site, &row.slug, &row.item).unwrap_or(false) {
                return Verdict::Skipped;
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
            return Verdict::Skipped;
        }
        let (state, error, wait) = match result {
            Ok(Outcome::Done) => (DownloadState::Done, None, None),
            Ok(Outcome::Stopped) => (DownloadState::Paused, None, None),
            // Пользователь нажал паузу, а задание упало раньше, чем заметило флажок:
            // пауза важнее ошибки, иначе задание само вернулось бы в работу.
            Err(_) if stop.load(Ordering::SeqCst) => (DownloadState::Paused, None, None),
            Err(e) => match rate_limit(&e) {
                // Задание не провалено: оно подождёт в очереди, сколько просил сервис.
                Some(wait) => (DownloadState::Queued, Some(format!("{e:#}")), Some(wait)),
                None => (DownloadState::Failed, Some(format!("{e:#}")), None),
            },
        };
        let _ = self
            .db
            .set_state(row.site, &row.slug, &row.item, state, error.as_deref());
        drop(running);
        self.emit_row(row.site, &row.slug, &row.item, false);
        match wait {
            Some(wait) => Verdict::Wait(wait),
            None => Verdict::Finished(state),
        }
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
        // Субтитры качаются до видео: когда файл серии готов, задание за ними уже
        // не вернётся. Поэтому временный сбой валит задание, и повтор возьмёт их.
        for sub in &video.subtitles {
            let path = target.with_extension(subtitle_ext(&sub.format));
            if path.is_file() {
                continue;
            }
            match self.client.fetch_bytes(Site::AnimeLib, &sub.url).await {
                Ok(bytes) => write_file(&path, &bytes).await?,
                // Файла на сервере нет: серия без субтитров лучше, чем никакой.
                Err(e) if subtitle_missing(&e) => eprintln!("субтитры {} пропущены: {e}", sub.url),
                Err(e) => return Err(anyhow::Error::new(e).context("субтитры")),
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

/// Чем кончилось задание для рабочего цикла.
enum Verdict {
    /// Задание досталось другому процессу или удалено.
    Skipped,
    Finished(DownloadState),
    /// Сервис попросил паузу, задание вернулось в очередь.
    Wait(Duration),
}

/// Сколько ждать, если сервис попросил паузу. Ищет по всей цепочке причин ошибки.
fn rate_limit(e: &anyhow::Error) -> Option<Duration> {
    e.chain()
        .find_map(|cause| match cause.downcast_ref::<libsocial::Error>() {
            Some(libsocial::Error::RateLimited { retry_after }) => Some(*retry_after),
            _ => None,
        })
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

/// Главы ранобэ – текст, а задание на главу качает картинки страниц. Пока задания
/// для текста нет (docs/roadmap.md), такое задание упало бы на запросе страниц.
fn refuse_ranobe(site: Site) -> Result<()> {
    if site.is_ranobe() {
        anyhow::bail!("скачивание ранобэ пока не поддерживается");
    }
    Ok(())
}

/// Ошибка говорит о самом файле субтитров, а не о сбое сети или сервера.
fn subtitle_missing(e: &libsocial::Error) -> bool {
    match e {
        libsocial::Error::NotFound | libsocial::Error::Forbidden => true,
        libsocial::Error::Http { status, .. } => status.is_client_error() && status.as_u16() != 429,
        _ => false,
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

/// Задания целиком: настоящий загрузчик против мок-сервера на месте API и серверов
/// картинок и видео.
#[cfg(test)]
mod job_tests {
    use std::sync::atomic::AtomicUsize;

    use libsocial::ClientConfig;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    const MANGA: &str = "1--test";
    const ANIME: &str = "2--test";
    const VIDEO_HREF: &str = "/v/ep1.mp4";

    struct Env {
        server: MockServer,
        db: Db,
        library: Library,
        downloader: Downloader,
        dir: PathBuf,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("polka-{name}-{}-{n}", std::process::id()))
    }

    /// Мок-сервер со справочником и загрузчик с одной картинкой за раз.
    async fn env(name: &str) -> Env {
        let server = MockServer::start().await;
        Mock::given(path("/api/constants"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "imageServers": [{"id": "main", "url": format!("{}/img", server.uri()), "site_ids": [1]}],
                "videoServers": [{"id": "main", "url": format!("{}/video", server.uri()), "site_ids": [5]}],
            }})))
            .mount(&server)
            .await;
        let client = Client::new(ClientConfig {
            api_mirrors: vec![format!("{}/api", server.uri())],
            api_interval: Duration::ZERO,
            ..Default::default()
        })
        .unwrap();
        let dir = temp_dir(name);
        let db = Db::in_memory().unwrap();
        let library = Library::new(dir.join("library"));
        let downloader = Downloader::start(
            client,
            db.clone(),
            library.clone(),
            1,
            "main".into(),
            &dir.join("downloader.lock"),
        );
        assert!(downloader.is_active());
        Env {
            server,
            db,
            library,
            downloader,
            dir,
        }
    }

    fn title(site: Site, slug: &str) -> TitleRow {
        TitleRow {
            site,
            slug: slug.into(),
            title_id: 1,
            name: "Тест".into(),
            cover_url: None,
            details: None,
            in_library: false,
        }
    }

    fn chapter_job() -> Job {
        Job::Chapter {
            chapter_id: 10,
            volume: "1".into(),
            number: "1".into(),
            branch_id: None,
            item_id: None,
        }
    }

    fn episode_job() -> Job {
        Job::Episode {
            episode_id: 41,
            player_id: 5,
            season: "1".into(),
            number: "1".into(),
            max_quality: 1080,
        }
    }

    /// JPEG больше порога заглушки, отличимый по номеру.
    fn jpeg(n: u8) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.resize(1000, n);
        bytes
    }

    fn video_bytes() -> Vec<u8> {
        (0..10_000u32).map(|i| (i % 251) as u8).collect()
    }

    /// Глава из `pages` страниц. Сервер сжатия отдаёт JPEG под именем `.png`.
    /// С `once` каждая страница должна быть запрошена ровно один раз.
    async fn mount_chapter(server: &MockServer, pages: u8, delay: Duration, once: bool) {
        let list: Vec<_> = (1..=pages)
            .map(|i| json!({"id": i, "slug": i, "url": format!("/p/{i}.png")}))
            .collect();
        Mock::given(path(format!("/api/manga/{MANGA}/chapter")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": 10, "volume": "1", "number": "1", "name": "Начало", "pages": list,
            }})))
            .mount(server)
            .await;
        for i in 1..=pages {
            let mock = Mock::given(path(format!("/img/p/{i}.png")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg(i)).set_delay(delay));
            let mock = if once { mock.expect(1) } else { mock };
            mock.mount(server).await;
        }
    }

    /// Серия с одной озвучкой плеера Animelib и субтитрами.
    async fn mount_episode(server: &MockServer) {
        Mock::given(path("/api/episodes/41"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": 41, "number": "1", "players": [{
                    "id": 5,
                    "player": "Animelib",
                    "video_domain": "main",
                    "video": {"id": 1, "quality": [{"href": VIDEO_HREF, "quality": 720}]},
                    "subtitles": [{"src": "/s/ep1.vtt", "format": "vtt"}],
                }],
            }})))
            .mount(server)
            .await;
        // Проверка сервера перед загрузкой.
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .and(header("range", "bytes=0-0"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(&video_bytes()[..1]))
            .with_priority(1)
            .mount(server)
            .await;
    }

    async fn mount_subtitles(server: &MockServer, response: ResponseTemplate) {
        Mock::given(path("/video/s/ep1.vtt"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn wait_for(db: &Db, site: Site, slug: &str, item: &str, check: impl Fn(Option<&DownloadRow>) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let row = db.download(site, slug, item).unwrap();
            if check(row.as_ref()) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "не дождался, задание: {row:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_state(db: &Db, site: Site, slug: &str, item: &str, state: DownloadState) -> DownloadRow {
        wait_for(db, site, slug, item, |r| r.is_some_and(|r| r.state == state)).await;
        db.download(site, slug, item).unwrap().unwrap()
    }

    /// Запросы к видео, кроме проверки сервера: значения заголовка Range.
    async fn video_ranges(server: &MockServer) -> Vec<Option<String>> {
        let requests: Vec<Request> = server.received_requests().await.unwrap();
        requests
            .iter()
            .filter(|r| r.url.path() == format!("/video{VIDEO_HREF}"))
            .map(|r| r.headers.get("range").map(|v| v.to_str().unwrap().to_string()))
            .filter(|r| r.as_deref() != Some("bytes=0-0"))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ranobe_is_refused() {
        let e = env("ranobe").await;
        let err = e
            .downloader
            .enqueue(&title(Site::RanobeLib, MANGA), "Глава 1", chapter_job())
            .unwrap_err();
        assert!(err.to_string().contains("ранобэ"), "{err}");
        assert!(e.db.downloads().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limit_keeps_job_queued() {
        let e = env("rate-limit").await;
        Mock::given(path(format!("/api/manga/{MANGA}/chapter")))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
            .expect(1)
            .mount(&e.server)
            .await;
        let job = chapter_job();
        e.downloader
            .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
            .unwrap();
        let item = job.item_key();
        // Задание вернулось в очередь с объяснением, а не провалилось.
        wait_for(&e.db, Site::MangaLib, MANGA, &item, |r| {
            r.is_some_and(|r| r.state == DownloadState::Queued && r.error.is_some())
        })
        .await;
        let row = e.db.download(Site::MangaLib, MANGA, &item).unwrap().unwrap();
        assert!(row.error.unwrap().contains("подождать"));
        // Пока идёт пауза, загрузчик к серверу не возвращается: expect(1) это проверит.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            e.db.download(Site::MangaLib, MANGA, &item).unwrap().unwrap().state,
            DownloadState::Queued
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chapter_downloads_with_marker() {
        let e = env("chapter").await;
        mount_chapter(&e.server, 3, Duration::ZERO, true).await;
        let job = chapter_job();
        assert!(
            e.downloader
                .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
                .unwrap()
        );
        // Второй раз то же задание не встаёт.
        assert!(
            !e.downloader
                .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
                .unwrap()
        );

        let row = wait_state(&e.db, Site::MangaLib, MANGA, &job.item_key(), DownloadState::Done).await;
        assert_eq!((row.done, row.total), (3, 3));
        let dir = e.library.job_path(Site::MangaLib, MANGA, &job);
        assert!(library::chapter_complete(&dir));
        let pages = library::chapter_pages(&dir).unwrap();
        let names: Vec<_> = pages.iter().map(|p| p.file_name().unwrap().to_str().unwrap()).collect();
        // Расширение по содержимому, а не по адресу.
        assert_eq!(names, ["0001.jpg", "0002.jpg", "0003.jpg"]);
        assert_eq!(std::fs::read(&pages[1]).unwrap(), jpeg(2));
        e.server.verify().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn not_an_image_fails_without_marker() {
        let e = env("not-image").await;
        Mock::given(path(format!("/api/manga/{MANGA}/chapter")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": 10, "volume": "1", "number": "1",
                "pages": [{"id": 1, "slug": 1, "url": "/p/1.png"}],
            }})))
            .mount(&e.server)
            .await;
        // Страница блокировки со статусом 200.
        Mock::given(path("/img/p/1.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<html>captcha</html>", "text/html"))
            .mount(&e.server)
            .await;
        let job = chapter_job();
        e.downloader
            .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
            .unwrap();

        let row = wait_state(&e.db, Site::MangaLib, MANGA, &job.item_key(), DownloadState::Failed).await;
        assert!(row.error.unwrap().contains("не картинку"));
        assert!(!library::chapter_complete(&e.library.job_path(
            Site::MangaLib,
            MANGA,
            &job
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pause_keeps_pages_and_resume_skips_them() {
        let e = env("pause").await;
        // Каждая страница по 300 мс, по одной за раз: пауза успевает прийти посреди главы.
        mount_chapter(&e.server, 3, Duration::from_millis(300), true).await;
        let job = chapter_job();
        let item = job.item_key();
        e.downloader
            .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
            .unwrap();
        wait_for(&e.db, Site::MangaLib, MANGA, &item, |r| {
            r.is_some_and(|r| r.state == DownloadState::Running && r.total > 0)
        })
        .await;
        e.downloader.pause(Site::MangaLib, MANGA, &item).unwrap();

        let row = wait_state(&e.db, Site::MangaLib, MANGA, &item, DownloadState::Paused).await;
        let dir = e.library.job_path(Site::MangaLib, MANGA, &job);
        assert!(row.done < 3, "пауза пришла слишком поздно: {row:?}");
        assert!(library::find_page(&dir, 0).is_some());
        assert!(!library::chapter_complete(&dir));

        e.downloader.resume(Site::MangaLib, MANGA, &item).unwrap();
        wait_state(&e.db, Site::MangaLib, MANGA, &item, DownloadState::Done).await;
        assert!(library::chapter_complete(&dir));
        // Каждая страница запрошена ровно один раз: скачанные не перекачивались.
        e.server.verify().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_while_running_deletes_files() {
        let e = env("remove").await;
        mount_chapter(&e.server, 3, Duration::from_millis(300), false).await;
        let job = chapter_job();
        let item = job.item_key();
        e.downloader
            .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
            .unwrap();
        wait_for(&e.db, Site::MangaLib, MANGA, &item, |r| {
            r.is_some_and(|r| r.state == DownloadState::Running && r.total > 0)
        })
        .await;
        e.downloader.remove(Site::MangaLib, MANGA, &item).unwrap();

        wait_for(&e.db, Site::MangaLib, MANGA, &item, |r| r.is_none()).await;
        assert!(!e.library.job_path(Site::MangaLib, MANGA, &job).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_process_only_enqueues() {
        let e = env("second").await;
        mount_chapter(&e.server, 1, Duration::ZERO, true).await;
        let other = Downloader::start(
            Client::new(ClientConfig::default()).unwrap(),
            e.db.clone(),
            e.library.clone(),
            1,
            "main".into(),
            &e.dir.join("downloader.lock"),
        );
        assert!(!other.is_active());
        // Задание из второго процесса выполняет тот, кто держит блокировку.
        let job = chapter_job();
        other
            .enqueue(&title(Site::MangaLib, MANGA), "Глава 1", job.clone())
            .unwrap();
        e.downloader.wake.notify_one();
        wait_state(&e.db, Site::MangaLib, MANGA, &job.item_key(), DownloadState::Done).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn episode_with_subtitles() {
        let e = env("episode").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(video_bytes()))
            .mount(&e.server)
            .await;
        let job = episode_job();
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        assert_eq!(std::fs::read(&target).unwrap(), video_bytes());
        assert_eq!(
            std::fs::read_to_string(target.with_extension("vtt")).unwrap(),
            "WEBVTT\n"
        );
        assert!(!part_path(&target).exists());
        assert!(!sidecar_path(&part_path(&target)).exists());
    }

    /// Кладёт недокачанный кусок видео с описанием, от какого он файла.
    fn put_part(target: &Path, bytes: &[u8], href: &str) {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let part = part_path(target);
        std::fs::write(&part, bytes).unwrap();
        let identity = PartIdentity {
            file: href.into(),
            quality: 720,
        };
        std::fs::write(sidecar_path(&part), serde_json::to_vec(&identity).unwrap()).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn episode_resumes_from_part() {
        let e = env("resume").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        let full = video_bytes();
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .and(header("range", "bytes=4000-"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(&full[4000..]))
            .mount(&e.server)
            .await;
        let job = episode_job();
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        put_part(&target, &full[..4000], VIDEO_HREF);
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        assert_eq!(std::fs::read(&target).unwrap(), full);
        assert_eq!(video_ranges(&e.server).await, [Some("bytes=4000-".to_string())]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn foreign_part_is_discarded() {
        let e = env("foreign").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(video_bytes()))
            .mount(&e.server)
            .await;
        let job = episode_job();
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        // Кусок другой озвучки: склеить его с этим файлом значит испортить серию.
        put_part(&target, &[0xAA; 4000], "/v/other.mp4");
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        assert_eq!(std::fs::read(&target).unwrap(), video_bytes());
        assert_eq!(video_ranges(&e.server).await, [None]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_without_ranges_restarts_from_zero() {
        let e = env("no-range").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        // На любой запрос отдаёт файл целиком со статусом 200.
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(video_bytes()))
            .mount(&e.server)
            .await;
        let job = episode_job();
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        put_part(&target, &video_bytes()[..4000], VIDEO_HREF);
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        // Без дозаписи в хвост старого куска.
        assert_eq!(std::fs::read(&target).unwrap(), video_bytes());
        assert_eq!(video_ranges(&e.server).await, [Some("bytes=4000-".to_string()), None]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn complete_part_is_finished_on_416() {
        let e = env("416").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        let full = video_bytes();
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .and(header("range", format!("bytes={}-", full.len()).as_str()))
            .respond_with(ResponseTemplate::new(416))
            .mount(&e.server)
            .await;
        let job = episode_job();
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        // Процесс прервали между последним куском и переименованием.
        put_part(&target, &full, VIDEO_HREF);
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        assert_eq!(std::fs::read(&target).unwrap(), full);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_subtitles_do_not_block_video() {
        let e = env("subs-404").await;
        mount_episode(&e.server).await;
        mount_subtitles(&e.server, ResponseTemplate::new(404)).await;
        Mock::given(path(format!("/video{VIDEO_HREF}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(video_bytes()))
            .mount(&e.server)
            .await;
        let job = episode_job();
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        wait_state(&e.db, Site::AnimeLib, ANIME, &job.item_key(), DownloadState::Done).await;
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        assert!(target.is_file());
        assert!(!target.with_extension("vtt").exists());
    }

    /// Медленный: клиент повторяет упавший запрос с паузами 2, 4 и 8 секунд.
    #[tokio::test(flavor = "multi_thread")]
    async fn subtitles_failure_is_retried_with_job() {
        let e = env("subs-500").await;
        mount_episode(&e.server).await;
        Mock::given(path("/video/s/ep1.vtt"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(4)
            .with_priority(1)
            .mount(&e.server)
            .await;
        mount_subtitles(&e.server, ResponseTemplate::new(200).set_body_string("WEBVTT\n")).await;
        Mock::given(method("GET"))
            .and(path(format!("/video{VIDEO_HREF}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(video_bytes()))
            .mount(&e.server)
            .await;
        let job = episode_job();
        let item = job.item_key();
        e.downloader
            .enqueue(&title(Site::AnimeLib, ANIME), "Серия 1", job.clone())
            .unwrap();

        let row = wait_state(&e.db, Site::AnimeLib, ANIME, &item, DownloadState::Failed).await;
        assert!(row.error.unwrap().contains("субтитры"));
        let target = e.library.job_path(Site::AnimeLib, ANIME, &job);
        // Видео не качалось: иначе повтор задания до субтитров бы не дошёл.
        assert!(!target.exists());

        e.downloader.resume(Site::AnimeLib, ANIME, &item).unwrap();
        wait_state(&e.db, Site::AnimeLib, ANIME, &item, DownloadState::Done).await;
        assert!(target.is_file());
        assert_eq!(
            std::fs::read_to_string(target.with_extension("vtt")).unwrap(),
            "WEBVTT\n"
        );
    }
}
