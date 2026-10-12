//! База приложения: библиотека тайтлов, очередь загрузок, прогресс чтения и просмотра.
//!
//! Соединение одно, под мьютексом. Запросы короткие, поэтому их можно звать прямо из
//! асинхронного кода, не уводя в отдельный поток.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use libsocial::Site;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Схема по версиям. Новая версия дописывается в конец, старые не меняются.
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE titles (
        site        TEXT    NOT NULL,
        slug        TEXT    NOT NULL,
        title_id    INTEGER NOT NULL,
        name        TEXT    NOT NULL,
        cover_url   TEXT,
        details     TEXT,
        in_library  INTEGER NOT NULL DEFAULT 0,
        added_at    INTEGER,
        updated_at  INTEGER NOT NULL,
        PRIMARY KEY (site, slug)
    );
    CREATE TABLE downloads (
        site        TEXT    NOT NULL,
        slug        TEXT    NOT NULL,
        item        TEXT    NOT NULL,
        label       TEXT    NOT NULL,
        sort        REAL    NOT NULL,
        state       TEXT    NOT NULL,
        done        INTEGER NOT NULL DEFAULT 0,
        total       INTEGER NOT NULL DEFAULT 0,
        error       TEXT,
        path        TEXT    NOT NULL,
        job         TEXT    NOT NULL,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL,
        PRIMARY KEY (site, slug, item)
    );
    CREATE INDEX downloads_state ON downloads (state, created_at);
    CREATE TABLE progress (
        site        TEXT    NOT NULL,
        slug        TEXT    NOT NULL,
        item        TEXT    NOT NULL,
        position    INTEGER NOT NULL,
        total       INTEGER NOT NULL,
        finished    INTEGER NOT NULL DEFAULT 0,
        updated_at  INTEGER NOT NULL,
        PRIMARY KEY (site, slug, item)
    );
    CREATE INDEX progress_recent ON progress (updated_at);
"#];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
}

impl DownloadState {
    pub fn as_str(self) -> &'static str {
        match self {
            DownloadState::Queued => "queued",
            DownloadState::Running => "running",
            DownloadState::Paused => "paused",
            DownloadState::Done => "done",
            DownloadState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> DownloadState {
        match s {
            "running" => DownloadState::Running,
            "paused" => DownloadState::Paused,
            "done" => DownloadState::Done,
            "failed" => DownloadState::Failed,
            _ => DownloadState::Queued,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TitleRow {
    pub site: Site,
    pub slug: String,
    pub title_id: u64,
    pub name: String,
    pub cover_url: Option<String>,
    pub details: Option<String>,
    pub in_library: bool,
}

#[derive(Debug, Clone)]
pub struct DownloadRow {
    pub site: Site,
    pub slug: String,
    pub item: String,
    pub label: String,
    pub sort: f64,
    pub state: DownloadState,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
    pub path: String,
    pub job: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub position: u64,
    pub total: u64,
    pub finished: bool,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("не открыть базу {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let version = version.max(0) as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))
                .with_context(|| format!("миграция базы до версии {}", i + 1))?;
        }
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("соединение с базой отравлено");
        Ok(f(&conn)?)
    }

    // --- Тайтлы ---

    /// Сохраняет карточку тайтла. Флаг «в библиотеке» не трогает.
    pub fn upsert_title(&self, row: &TitleRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO titles (site, slug, title_id, name, cover_url, details, in_library, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (site, slug) DO UPDATE SET
                    title_id = excluded.title_id, name = excluded.name,
                    cover_url = COALESCE(excluded.cover_url, cover_url),
                    details = COALESCE(excluded.details, details),
                    updated_at = excluded.updated_at",
                params![
                    row.site.key(),
                    row.slug,
                    row.title_id as i64,
                    row.name,
                    row.cover_url,
                    row.details,
                    row.in_library,
                    now()
                ],
            )
            .map(|_| ())
        })
    }

    pub fn set_in_library(&self, site: Site, slug: &str, value: bool) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE titles SET in_library = ?3,
                    added_at = CASE WHEN ?3 THEN COALESCE(added_at, ?4) ELSE added_at END
                 WHERE site = ?1 AND slug = ?2",
                params![site.key(), slug, value, now()],
            )
            .map(|_| ())
        })
    }

    pub fn title(&self, site: Site, slug: &str) -> Result<Option<TitleRow>> {
        self.with(|c| {
            c.query_row(
                "SELECT site, slug, title_id, name, cover_url, details, in_library
                 FROM titles WHERE site = ?1 AND slug = ?2",
                params![site.key(), slug],
                title_row,
            )
            .optional()
        })
        .map(Option::flatten)
    }

    /// Тайтлы в библиотеке и тайтлы, у которых есть скачанное, новые сверху.
    pub fn library(&self) -> Result<Vec<TitleRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT t.site, t.slug, t.title_id, t.name, t.cover_url, t.details, t.in_library
                 FROM titles t
                 WHERE t.in_library = 1
                    OR EXISTS (SELECT 1 FROM downloads d WHERE d.site = t.site AND d.slug = t.slug)
                 ORDER BY COALESCE(t.added_at, t.updated_at) DESC",
            )?;
            let rows = st.query_map([], title_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().flatten().collect())
        })
    }

    // --- Загрузки ---

    /// Ставит загрузку в очередь. Уже скачанное и уже стоящее в очереди не трогает,
    /// упавшее или остановленное перезапускает.
    pub fn enqueue(&self, row: &DownloadRow) -> Result<bool> {
        self.with(|c| {
            let changed = c.execute(
                "INSERT INTO downloads (site, slug, item, label, sort, state, done, total, path, job, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 0, ?6, ?7, ?8, ?9, ?9)
                 ON CONFLICT (site, slug, item) DO UPDATE SET
                    state = 'queued', error = NULL, job = excluded.job, updated_at = excluded.updated_at
                 WHERE downloads.state IN ('failed', 'paused')",
                params![
                    row.site.key(),
                    row.slug,
                    row.item,
                    row.label,
                    row.sort,
                    row.total as i64,
                    row.path,
                    row.job,
                    now()
                ],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn set_state(
        &self,
        site: Site,
        slug: &str,
        item: &str,
        state: DownloadState,
        error: Option<&str>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE downloads SET state = ?4, error = ?5, updated_at = ?6
                 WHERE site = ?1 AND slug = ?2 AND item = ?3",
                params![site.key(), slug, item, state.as_str(), error, now()],
            )
            .map(|_| ())
        })
    }

    pub fn set_download_progress(&self, site: Site, slug: &str, item: &str, done: u64, total: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE downloads SET done = ?4, total = ?5, updated_at = ?6
                 WHERE site = ?1 AND slug = ?2 AND item = ?3",
                params![site.key(), slug, item, done as i64, total as i64, now()],
            )
            .map(|_| ())
        })
    }

    pub fn remove_download(&self, site: Site, slug: &str, item: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM downloads WHERE site = ?1 AND slug = ?2 AND item = ?3",
                params![site.key(), slug, item],
            )
            .map(|_| ())
        })
    }

    /// Следующая загрузка из очереди, самая старая.
    pub fn next_queued(&self) -> Result<Option<DownloadRow>> {
        self.with(|c| {
            c.query_row(
                &format!(
                    "SELECT {DOWNLOAD_COLS} FROM downloads WHERE state = 'queued' ORDER BY created_at, sort LIMIT 1"
                ),
                [],
                download_row,
            )
            .optional()
        })
        .map(Option::flatten)
    }

    /// После падения приложения «выполняется» значит «прервано»: возвращаем в очередь.
    pub fn requeue_interrupted(&self) -> Result<usize> {
        self.with(|c| c.execute("UPDATE downloads SET state = 'queued' WHERE state = 'running'", []))
    }

    pub fn downloads(&self) -> Result<Vec<DownloadRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {DOWNLOAD_COLS} FROM downloads ORDER BY
                    CASE state WHEN 'running' THEN 0 WHEN 'queued' THEN 1 WHEN 'paused' THEN 2
                               WHEN 'failed' THEN 3 ELSE 4 END, created_at, sort"
            ))?;
            let rows = st.query_map([], download_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().flatten().collect())
        })
    }

    pub fn downloads_for(&self, site: Site, slug: &str) -> Result<Vec<DownloadRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {DOWNLOAD_COLS} FROM downloads WHERE site = ?1 AND slug = ?2 ORDER BY sort"
            ))?;
            let rows = st
                .query_map(params![site.key(), slug], download_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().flatten().collect())
        })
    }

    // --- Прогресс ---

    pub fn save_progress(&self, site: Site, slug: &str, item: &str, p: Progress) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO progress (site, slug, item, position, total, finished, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (site, slug, item) DO UPDATE SET
                    position = excluded.position, total = excluded.total,
                    finished = MAX(finished, excluded.finished), updated_at = excluded.updated_at",
                params![
                    site.key(),
                    slug,
                    item,
                    p.position as i64,
                    p.total as i64,
                    p.finished,
                    now()
                ],
            )
            .map(|_| ())
        })
    }

    pub fn progress(&self, site: Site, slug: &str, item: &str) -> Result<Option<Progress>> {
        self.with(|c| {
            c.query_row(
                "SELECT position, total, finished FROM progress WHERE site = ?1 AND slug = ?2 AND item = ?3",
                params![site.key(), slug, item],
                progress_row,
            )
            .optional()
        })
    }

    /// Прогресс по всем главам или эпизодам тайтла: ключ элемента → прогресс.
    pub fn progress_for(&self, site: Site, slug: &str) -> Result<Vec<(String, Progress)>> {
        self.with(|c| {
            let mut st =
                c.prepare("SELECT item, position, total, finished FROM progress WHERE site = ?1 AND slug = ?2")?;
            st.query_map(params![site.key(), slug], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Progress {
                        position: r.get::<_, i64>(1)? as u64,
                        total: r.get::<_, i64>(2)? as u64,
                        finished: r.get(3)?,
                    },
                ))
            })?
            .collect()
        })
    }

    /// Последний открытый элемент тайтла, чтобы предложить «продолжить».
    pub fn last_item(&self, site: Site, slug: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row(
                "SELECT item FROM progress WHERE site = ?1 AND slug = ?2 ORDER BY updated_at DESC LIMIT 1",
                params![site.key(), slug],
                |r| r.get(0),
            )
            .optional()
        })
    }
}

const DOWNLOAD_COLS: &str = "site, slug, item, label, sort, state, done, total, error, path, job";

/// Строка с неизвестным сайтом (из будущей версии) пропускается.
fn title_row(r: &rusqlite::Row) -> rusqlite::Result<Option<TitleRow>> {
    let Some(site) = Site::from_key(&r.get::<_, String>(0)?) else {
        return Ok(None);
    };
    Ok(Some(TitleRow {
        site,
        slug: r.get(1)?,
        title_id: r.get::<_, i64>(2)? as u64,
        name: r.get(3)?,
        cover_url: r.get(4)?,
        details: r.get(5)?,
        in_library: r.get(6)?,
    }))
}

fn download_row(r: &rusqlite::Row) -> rusqlite::Result<Option<DownloadRow>> {
    let Some(site) = Site::from_key(&r.get::<_, String>(0)?) else {
        return Ok(None);
    };
    Ok(Some(DownloadRow {
        site,
        slug: r.get(1)?,
        item: r.get(2)?,
        label: r.get(3)?,
        sort: r.get(4)?,
        state: DownloadState::parse(&r.get::<_, String>(5)?),
        done: r.get::<_, i64>(6)? as u64,
        total: r.get::<_, i64>(7)? as u64,
        error: r.get(8)?,
        path: r.get(9)?,
        job: r.get(10)?,
    }))
}

fn progress_row(r: &rusqlite::Row) -> rusqlite::Result<Progress> {
    Ok(Progress {
        position: r.get::<_, i64>(0)? as u64,
        total: r.get::<_, i64>(1)? as u64,
        finished: r.get(2)?,
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(item: &str) -> DownloadRow {
        DownloadRow {
            site: Site::MangaLib,
            slug: "1--x".into(),
            item: item.into(),
            label: item.into(),
            sort: 1.0,
            state: DownloadState::Queued,
            done: 0,
            total: 0,
            error: None,
            path: "/tmp/x".into(),
            job: "{}".into(),
        }
    }

    #[test]
    fn queue_lifecycle() {
        let db = Db::in_memory().unwrap();
        assert!(db.enqueue(&download("c1")).unwrap());
        // Повторная постановка уже стоящей в очереди ничего не меняет.
        assert!(!db.enqueue(&download("c1")).unwrap());
        let next = db.next_queued().unwrap().unwrap();
        assert_eq!(next.item, "c1");

        db.set_state(Site::MangaLib, "1--x", "c1", DownloadState::Running, None)
            .unwrap();
        assert!(db.next_queued().unwrap().is_none());
        assert_eq!(db.requeue_interrupted().unwrap(), 1);

        db.set_state(Site::MangaLib, "1--x", "c1", DownloadState::Done, None)
            .unwrap();
        // Скачанное заново в очередь не встаёт.
        assert!(!db.enqueue(&download("c1")).unwrap());
        db.set_state(Site::MangaLib, "1--x", "c1", DownloadState::Failed, Some("сеть"))
            .unwrap();
        assert!(db.enqueue(&download("c1")).unwrap());
        assert!(db.downloads().unwrap()[0].error.is_none());
    }

    #[test]
    fn library_and_progress() {
        let db = Db::in_memory().unwrap();
        let row = TitleRow {
            site: Site::MangaLib,
            slug: "1--x".into(),
            title_id: 1,
            name: "Икс".into(),
            cover_url: None,
            details: None,
            in_library: false,
        };
        db.upsert_title(&row).unwrap();
        assert!(db.library().unwrap().is_empty());
        db.set_in_library(Site::MangaLib, "1--x", true).unwrap();
        assert_eq!(db.library().unwrap().len(), 1);

        let p = Progress {
            position: 5,
            total: 20,
            finished: false,
        };
        db.save_progress(Site::MangaLib, "1--x", "c1", p).unwrap();
        db.save_progress(Site::MangaLib, "1--x", "c1", Progress { finished: true, ..p })
            .unwrap();
        // Прочитанная глава не становится непрочитанной при повторном открытии.
        db.save_progress(Site::MangaLib, "1--x", "c1", p).unwrap();
        assert!(db.progress(Site::MangaLib, "1--x", "c1").unwrap().unwrap().finished);
        assert_eq!(db.last_item(Site::MangaLib, "1--x").unwrap().as_deref(), Some("c1"));
    }
}
