//! Раскладка библиотеки на диске и экспорт в CBZ.
//!
//! ```text
//! library/<сайт>/<slug>/title.json        карточка тайтла, чтобы библиотека жила без сети
//! library/<сайт>/<slug>/cover.jpg
//! library/<сайт>/<slug>/v1-c12/0001.jpg   страницы главы
//! library/<сайт>/<slug>/v1-c12/chapter.json  пишется последним: глава скачана целиком
//! library/animelib/<slug>/s1-e3-p5371.mp4 эпизод, рядом субтитры .vtt/.ass
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use libsocial::Site;
use libsocial::models::{Chapter, Episode, Player};
use serde::{Deserialize, Serialize};

pub const CHAPTER_MARKER: &str = "chapter.json";

/// Что именно скачать. Хранится в базе вместе с записью очереди.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Job {
    Chapter {
        chapter_id: u64,
        volume: String,
        number: String,
        branch_id: Option<u64>,
    },
    Episode {
        episode_id: u64,
        player_id: u64,
        season: String,
        number: String,
        max_quality: u64,
    },
}

impl Job {
    pub fn chapter(chapter: &Chapter, branch_id: Option<u64>) -> Job {
        Job::Chapter {
            chapter_id: chapter.id,
            volume: chapter.volume.clone(),
            number: chapter.number.clone(),
            branch_id,
        }
    }

    pub fn episode(episode: &Episode, player: &Player, max_quality: u64) -> Job {
        Job::Episode {
            episode_id: episode.id,
            player_id: player.id,
            season: episode.season.clone(),
            number: episode.number.clone(),
            max_quality,
        }
    }

    /// Ключ элемента в базе. Тот же ключ использует прогресс чтения.
    pub fn item_key(&self) -> String {
        match self {
            Job::Chapter {
                chapter_id, branch_id, ..
            } => chapter_key(*chapter_id, *branch_id),
            Job::Episode { episode_id, .. } => episode_key(*episode_id),
        }
    }

    /// Порядок внутри тайтла: том и глава или сезон и серия.
    pub fn sort(&self) -> f64 {
        let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        match self {
            Job::Chapter { volume, number, .. } => num(volume) * 10_000.0 + num(number),
            Job::Episode { season, number, .. } => num(season) * 10_000.0 + num(number),
        }
    }
}

pub fn chapter_key(chapter_id: u64, branch_id: Option<u64>) -> String {
    format!("c{chapter_id}-b{}", branch_id.unwrap_or(0))
}

/// Прогресс просмотра не зависит от озвучки, поэтому ключ без плеера.
pub fn episode_key(episode_id: u64) -> String {
    format!("e{episode_id}")
}

#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn new(root: PathBuf) -> Library {
        Library { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn title_dir(&self, site: Site, slug: &str) -> PathBuf {
        self.root.join(site.key()).join(safe(slug))
    }

    /// Где лежит результат задания: каталог главы или файл эпизода.
    pub fn job_path(&self, site: Site, slug: &str, job: &Job) -> PathBuf {
        let dir = self.title_dir(site, slug);
        match job {
            // В имени и номер главы для человека, и её id: на сайте бывают две
            // разные главы с одинаковыми томом и номером.
            Job::Chapter {
                chapter_id,
                volume,
                number,
                branch_id,
            } => {
                let branch = branch_id.map(|b| format!("-b{b}")).unwrap_or_default();
                dir.join(safe(&format!("v{volume}-c{number}{branch}-{chapter_id}")))
            }
            Job::Episode {
                season,
                number,
                player_id,
                ..
            } => dir.join(safe(&format!("s{season}-e{number}-p{player_id}.mp4"))),
        }
    }

    /// Сохраняет карточку тайтла рядом с главами.
    pub fn write_title_meta(&self, site: Site, slug: &str, details: &serde_json::Value) -> Result<()> {
        let dir = self.title_dir(site, slug);
        std::fs::create_dir_all(&dir)?;
        crate::config::write_atomic(&dir.join("title.json"), &serde_json::to_vec_pretty(details)?)
    }

    pub fn cover_path(&self, site: Site, slug: &str) -> PathBuf {
        self.title_dir(site, slug).join("cover.jpg")
    }
}

/// Скачанная глава целиком: есть отметка о завершении.
pub fn chapter_complete(dir: &Path) -> bool {
    dir.join(CHAPTER_MARKER).is_file()
}

/// Страницы скачанной главы по порядку.
pub fn chapter_pages(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut pages: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("нет главы {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| is_page_file(p))
        .collect();
    pages.sort();
    Ok(pages)
}

/// Имя файла страницы: `0001.jpg`. Временные `.part` и метаданные не подходят.
pub fn page_file_name(index: usize, ext: &str) -> String {
    format!("{:04}.{}", index + 1, ext)
}

const PAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "avif"];

/// Уже скачанная страница с этим номером, с любым расширением.
pub fn find_page(dir: &Path, index: usize) -> Option<PathBuf> {
    PAGE_EXTS
        .iter()
        .map(|ext| dir.join(page_file_name(index, ext)))
        .find(|p| p.is_file())
}

fn is_page_file(p: &Path) -> bool {
    let stem_ok = p
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()));
    let ext_ok = p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif"
        )
    });
    stem_ok && ext_ok
}

/// Глава для экспорта: каталог и подпись.
pub struct ExportChapter {
    pub dir: PathBuf,
    pub volume: String,
    pub number: String,
}

/// Собирает главы в один CBZ. Картинки уже сжаты, поэтому кладутся без сжатия.
/// Недокачанная глава в экспорт не попадает: это ошибка, а не тихий пропуск.
pub fn export_cbz(series: &str, chapters: &[ExportChapter], out: &Path) -> Result<()> {
    let first = chapters.first().context("нечего экспортировать")?;
    for chapter in chapters {
        if !chapter_complete(&chapter.dir) {
            anyhow::bail!("глава {} скачана не полностью", chapter.number);
        }
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = out.with_extension("cbz.part");
    let result = write_cbz(series, first, chapters, &tmp);
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, out)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn write_cbz(series: &str, first: &ExportChapter, chapters: &[ExportChapter], tmp: &Path) -> Result<()> {
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(tmp).with_context(|| format!("не создать {}", tmp.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let number = if chapters.len() == 1 {
        first.number.clone()
    } else {
        format!(
            "{}-{}",
            first.number,
            chapters.last().map(|c| c.number.as_str()).unwrap_or("")
        )
    };
    let info = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ComicInfo>\n  <Series>{}</Series>\n  <Volume>{}</Volume>\n  <Number>{}</Number>\n  <LanguageISO>ru</LanguageISO>\n</ComicInfo>\n",
        xml_escape(series),
        xml_escape(&first.volume),
        xml_escape(&number)
    );
    zip.start_file("ComicInfo.xml", SimpleFileOptions::default())?;
    zip.write_all(info.as_bytes())?;
    for (ci, chapter) in chapters.iter().enumerate() {
        for (pi, page) in chapter_pages(&chapter.dir)?.iter().enumerate() {
            let ext = page.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
            zip.start_file(format!("{:04}-{:04}.{ext}", ci + 1, pi + 1), stored)?;
            std::io::copy(&mut std::fs::File::open(page)?, &mut zip)?;
        }
    }
    zip.finish()?;
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Делает строку безопасной для имени файла: без разделителей путей и точек в начале.
pub fn safe(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_start_matches('.').trim();
    if trimmed.is_empty() {
        return "_".into();
    }
    // Предел имени файла 255 байт, а кириллица занимает по два: режем по байтам,
    // оставляя место под расширение и приставки.
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe("../etc/passwd"), "_etc_passwd");
        assert_eq!(safe("7965--chainsaw-man"), "7965--chainsaw-man");
        assert_eq!(safe(".."), "_");
        assert!(!safe("a/b").contains('/'));
        assert!(safe(&"я".repeat(300)).len() <= 200);
    }

    #[test]
    fn paths_and_keys() {
        let lib = Library::new(PathBuf::from("/lib"));
        let job = Job::Chapter {
            chapter_id: 10,
            volume: "1".into(),
            number: "12.5".into(),
            branch_id: Some(3),
        };
        assert_eq!(
            lib.job_path(Site::MangaLib, "1--x", &job),
            PathBuf::from("/lib/mangalib/1--x/v1-c12.5-b3-10")
        );
        assert_eq!(job.item_key(), "c10-b3");
        assert!(job.sort() > 10_012.0 && job.sort() < 10_013.0);
        let json = serde_json::to_string(&job).unwrap();
        assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), job);
    }

    #[test]
    fn cbz_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("polka-cbz-{}", std::process::id()));
        let ch = tmp.join("v1-c1");
        std::fs::create_dir_all(&ch).unwrap();
        std::fs::write(ch.join("0002.png"), b"two").unwrap();
        std::fs::write(ch.join("0001.jpg"), b"one").unwrap();
        std::fs::write(ch.join(CHAPTER_MARKER), b"{}").unwrap();
        std::fs::write(ch.join("0003.jpg.part"), b"half").unwrap();
        assert_eq!(chapter_pages(&ch).unwrap().len(), 2);

        let out = tmp.join("out.cbz");
        let chapters = [ExportChapter {
            dir: ch.clone(),
            volume: "1".into(),
            number: "1".into(),
        }];
        export_cbz("Икс & Игрек", &chapters, &out).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        assert_eq!(names, ["ComicInfo.xml", "0001-0001.jpg", "0001-0002.png"]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
