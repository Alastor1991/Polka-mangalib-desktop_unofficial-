//! Запуск видео во внешнем плеере. Основной – mpv: он умеет заголовки запросов,
//! внешние субтитры и старт с нужной секунды.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use libsocial::{ResolvedVideo, Site};

fn has_mpv() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("mpv").is_file()))
        .unwrap_or(false)
}

/// Играет видео из сети. Серверу нужен Referer сайта, поэтому без mpv не обойтись.
pub async fn play_stream(video: ResolvedVideo, start: Option<u64>, title: String) -> Result<String, String> {
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
    cmd.arg(&video.url);
    spawn(cmd)?;
    Ok(format!("Открываю в mpv, качество {}p", video.quality))
}

/// Играет скачанный файл: в mpv, а если его нет, в плеере по умолчанию.
pub async fn play_file(path: PathBuf, start: Option<u64>, title: String) -> Result<String, String> {
    if !has_mpv() {
        open::that_detached(&path).map_err(|e| e.to_string())?;
        return Ok("Открываю в плеере по умолчанию".into());
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
    cmd.arg(&path);
    spawn(cmd)?;
    Ok("Открываю в mpv".into())
}

fn spawn(mut cmd: Command) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            // Дожидаемся выхода в стороннем потоке, чтобы не копить зомби-процессы.
            std::thread::spawn(move || child.wait());
        })
        .map_err(|e| format!("mpv не запустился: {e}"))
}
