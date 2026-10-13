//! Запуск видео во внешнем плеере. Основной – mpv: он умеет заголовки запросов,
//! внешние субтитры и старт с нужной секунды, а через свой IPC-сокет сообщает,
//! где остановился зритель.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use libsocial::{ResolvedVideo, Site};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::db::{Db, Progress};
use crate::sync::{Mark, Sync};

/// Серия считается просмотренной, когда позади девять десятых: эндинг и превью
/// следующей серии досматривают не все.
const WATCHED_SHARE: f64 = 0.9;
/// Как часто записывать позицию в базу во время просмотра.
const SAVE_EVERY: Duration = Duration::from_secs(10);
/// Сколько ждать, пока mpv откроет свой сокет.
const SOCKET_WAIT: Duration = Duration::from_secs(15);

/// Куда записывать позицию просмотра и что отправить на сайт.
pub struct Track {
    pub db: Db,
    pub sync: Sync,
    pub site: Site,
    pub slug: String,
    pub key: String,
    pub anime_id: u64,
    pub episode_id: u64,
    pub item_number: Option<u64>,
    /// Плеер, в котором смотрят: ему сайт ставит отметку «просмотрено».
    pub player_id: Option<u64>,
    /// Плеер, команда и перевод для закладки. Без них сайт место не примет.
    pub choice: Option<libsocial::PlayerChoice>,
}

impl Track {
    /// Плеер закрыт: место, где остановились, уходит в закладку на сайте.
    fn finish(&self) {
        let Ok(Some(progress)) = self.db.progress(self.site, &self.slug, &self.key) else {
            return;
        };
        self.sync.push(Mark::Place {
            site: self.site,
            slug: self.slug.clone(),
            place: libsocial::Place {
                item_id: self.episode_id,
                item_number: self.item_number,
                progress: libsocial::bookmarks::format_timecode(progress.position),
                player: self.choice.clone(),
            },
        });
    }
}

/// Запущенный плеер.
pub struct Playback {
    /// Что сказать пользователю о запуске.
    pub message: String,
    /// mpv и его сокет. Плеер по умолчанию открывается отдельно, следить там не за чем.
    mpv: Option<(Child, PathBuf)>,
}

impl Playback {
    /// Ждёт закрытия плеера и по ходу записывает позицию в базу.
    pub async fn track(self, track: Track) {
        let Some((mut child, socket)) = self.mpv else {
            return;
        };
        if let Some(stream) = connect(&socket, &mut child).await {
            watch(stream, |pos, duration| save(&track, pos, duration)).await;
            track.finish();
        }
        let _ = child.wait().await;
        let _ = std::fs::remove_file(&socket);
    }
}

fn has_mpv() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("mpv").is_file()))
        .unwrap_or(false)
}

/// Играет видео из сети. Серверу нужен Referer сайта, поэтому без mpv не обойтись.
pub async fn play_stream(video: ResolvedVideo, start: Option<u64>, title: String) -> Result<Playback, String> {
    if !video.url.starts_with("https://") && !video.url.starts_with("http://") {
        return Err("сервер прислал странный адрес видео, не открываю".into());
    }
    if !has_mpv() {
        return Err("Для просмотра нужен mpv: sudo pacman -S mpv. Пока можно скачать серию или открыть Kodik.".into());
    }
    let referer = format!("{}/", Site::AnimeLib.origin());
    let mut cmd = Command::new("mpv");
    cmd.arg(format!("--force-media-title={title}"))
        .arg(format!("--http-header-fields=Referer: {referer}"))
        .arg(format!("--user-agent=Polka/{}", env!("CARGO_PKG_VERSION")));
    for sub in &video.subtitles {
        cmd.arg(format!("--sub-file={}", sub.url));
    }
    if let Some(sec) = start.filter(|s| *s > 0) {
        cmd.arg(format!("--start={sec}"));
    }
    spawn_mpv(
        cmd,
        video.url.as_ref(),
        format!("Открываю в mpv, качество {}p", video.quality),
    )
}

/// Играет скачанный файл: в mpv, а если его нет, в плеере по умолчанию.
/// Позицию узнать можно только у mpv.
pub async fn play_file(path: PathBuf, start: Option<u64>, title: String) -> Result<Playback, String> {
    if !has_mpv() {
        open::that_detached(&path).map_err(|e| e.to_string())?;
        return Ok(Playback {
            message: "Открываю в плеере по умолчанию".into(),
            mpv: None,
        });
    }
    let mut cmd = Command::new("mpv");
    cmd.arg(format!("--force-media-title={title}"));
    if let Some(sec) = start.filter(|s| *s > 0) {
        cmd.arg(format!("--start={sec}"));
    }
    if let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) {
        for ext in ["ass", "vtt", "srt"] {
            let sub = dir.join(format!("{stem}.{ext}"));
            if sub.is_file() {
                cmd.arg(format!("--sub-file={}", sub.display()));
            }
        }
    }
    spawn_mpv(cmd, path.as_os_str(), "Открываю в mpv".into())
}

/// Запускает mpv с сокетом. Выхода дожидается `Playback::track`, иначе копились бы
/// зомби-процессы.
fn spawn_mpv(mut cmd: Command, target: &std::ffi::OsStr, message: String) -> Result<Playback, String> {
    let socket = socket_path();
    let _ = std::fs::remove_file(&socket);
    let child = cmd
        .arg(format!("--input-ipc-server={}", socket.display()))
        // `--` до адреса: строка из ответа сервера не должна стать опцией mpv.
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("mpv не запустился: {e}"))?;
    Ok(Playback {
        message,
        mpv: Some((child, socket)),
    })
}

/// Свой сокет на каждый запуск: можно смотреть две серии в двух окнах.
fn socket_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let n = N.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("polka-mpv-{}-{n}.sock", std::process::id()))
}

/// Подключается к сокету, как только mpv его откроет. `None`, если плеер успел
/// закрыться или так и не открыл сокет.
async fn connect(socket: &Path, child: &mut Child) -> Option<tokio::net::UnixStream> {
    let deadline = tokio::time::Instant::now() + SOCKET_WAIT;
    loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
            return Some(stream);
        }
        if !matches!(child.try_wait(), Ok(None)) || tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Подписывается на позицию и длительность и читает события, пока mpv не закроет
/// сокет. `save` получает позицию и длительность в секундах: раз в `SAVE_EVERY`
/// и последний раз при закрытии.
async fn watch<S>(stream: S, mut save: impl FnMut(f64, f64))
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let observe = concat!(
        r#"{"command": ["observe_property", 1, "time-pos"]}"#,
        "\n",
        r#"{"command": ["observe_property", 2, "duration"]}"#,
        "\n",
    );
    if write.write_all(observe.as_bytes()).await.is_err() {
        return;
    }
    let mut lines = BufReader::new(read).lines();
    let (mut pos, mut duration) = (None::<f64>, 0.0);
    let mut saved_at = tokio::time::Instant::now();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["event"] != "property-change" {
            continue;
        }
        // Перед закрытием mpv присылает пустые значения: их пропускаем, иначе
        // последней записалась бы нулевая позиция.
        let Some(value) = event["data"].as_f64() else {
            continue;
        };
        match event["name"].as_str() {
            Some("time-pos") => pos = Some(value.max(0.0)),
            Some("duration") => duration = value,
            _ => continue,
        }
        if let Some(pos) = pos
            && saved_at.elapsed() >= SAVE_EVERY
        {
            save(pos, duration);
            saved_at = tokio::time::Instant::now();
        }
    }
    if let Some(pos) = pos {
        save(pos, duration);
    }
}

fn save(track: &Track, pos: f64, duration: f64) {
    let progress = Progress {
        position: pos as u64,
        total: duration as u64,
        finished: duration > 0.0 && pos >= duration * WATCHED_SHARE,
    };
    let was_finished = track
        .db
        .progress(track.site, &track.slug, &track.key)
        .ok()
        .flatten()
        .is_some_and(|p| p.finished);
    if let Err(e) = track.db.save_progress(track.site, &track.slug, &track.key, progress) {
        eprintln!("не удалось записать позицию просмотра: {e:#}");
    }
    // На сайт отмечаем один раз, когда серия досмотрена впервые.
    if progress.finished
        && !was_finished
        && let Some(player_id) = track.player_id
    {
        track.sync.push(Mark::EpisodeViewed {
            anime_id: track.anime_id,
            player_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Подставной mpv: проверяет подписку, присылает события и закрывает сокет.
    async fn fake_mpv(events: &[&str]) -> Vec<(f64, f64)> {
        let (ours, mut mpv) = tokio::io::duplex(4096);
        let script: Vec<String> = events.iter().map(|e| format!("{e}\n")).collect();
        let server = tokio::spawn(async move {
            let mut request = vec![0u8; 512];
            let n = tokio::io::AsyncReadExt::read(&mut mpv, &mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]).into_owned();
            for line in script {
                mpv.write_all(line.as_bytes()).await.unwrap();
            }
            request
        });
        let mut saved = Vec::new();
        watch(ours, |pos, duration| saved.push((pos, duration))).await;
        let request = server.await.unwrap();
        assert!(request.contains("time-pos") && request.contains("duration"));
        saved
    }

    #[tokio::test]
    async fn last_position_saved_on_close() {
        let saved = fake_mpv(&[
            r#"{"event": "property-change", "id": 2, "name": "duration", "data": 1440.0}"#,
            r#"{"request_id": 0, "error": "success"}"#,
            r#"{"event": "property-change", "id": 1, "name": "time-pos", "data": 600.5}"#,
            "не json",
            r#"{"event": "property-change", "id": 1, "name": "time-pos"}"#,
            r#"{"event": "end-file"}"#,
        ])
        .await;
        assert_eq!(saved.last(), Some(&(600.5, 1440.0)));
    }

    #[tokio::test]
    async fn nothing_saved_without_position() {
        let saved = fake_mpv(&[r#"{"event": "property-change", "id": 2, "name": "duration", "data": 1440.0}"#]).await;
        assert!(saved.is_empty());
    }

    #[test]
    fn watched_near_the_end() {
        let db = Db::in_memory().unwrap();
        let track = Track {
            db: db.clone(),
            sync: Sync::detached(db.clone()),
            site: Site::AnimeLib,
            slug: "1--x".into(),
            key: "e1".into(),
            anime_id: 1,
            episode_id: 41,
            item_number: Some(1),
            player_id: Some(5),
            choice: None,
        };
        save(&track, 700.0, 1440.0);
        let p = db.progress(Site::AnimeLib, "1--x", "e1").unwrap().unwrap();
        assert_eq!((p.position, p.total, p.finished), (700, 1440, false));
        assert!(db.pending_marks(10).unwrap().is_empty());
        save(&track, 1350.0, 1440.0);
        assert!(db.progress(Site::AnimeLib, "1--x", "e1").unwrap().unwrap().finished);
        // Отметка уходит один раз, повторный конец серии её не множит.
        save(&track, 1400.0, 1440.0);
        track.finish();
        let marks: Vec<String> = db.pending_marks(10).unwrap().into_iter().map(|(_, m, _)| m).collect();
        assert_eq!(marks.len(), 2, "{marks:?}");
        assert!(
            marks
                .iter()
                .any(|m| m.contains("episode_viewed") && m.contains("\"player_id\":5"))
        );
        assert!(marks.iter().any(|m| m.contains("\"place\"") && m.contains("23:20")));
    }
}
