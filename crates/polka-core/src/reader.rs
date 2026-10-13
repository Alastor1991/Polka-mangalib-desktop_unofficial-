//! Читалка без интерфейса: какую главу открыть, откуда брать страницы, где
//! продолжить, что записать в базу и отправить на сайт.

use std::collections::HashMap;
use std::path::PathBuf;

use libsocial::models::{Chapter, PageImage};
use libsocial::{Place, Site};

use crate::context::Core;
use crate::db::Progress;
use crate::library::{self, chapter_key};
use crate::sync::Mark;

/// С чего открыть читалку. Главы идут от первой к последней.
#[derive(Debug, Clone)]
pub struct ReaderStart {
    pub site: Site,
    pub slug: String,
    /// Числовой id тайтла: с ним сайт принимает отметки о прочтении.
    pub title_id: u64,
    pub title: String,
    pub chapters: Vec<Chapter>,
    /// Выбранная ветка перевода для каждой главы.
    pub branches: HashMap<u64, Option<u64>>,
    /// Каталоги глав, скачанных целиком.
    pub local: HashMap<u64, PathBuf>,
    pub index: usize,
    pub offline: bool,
}

/// Откуда берётся страница.
#[derive(Debug, Clone)]
pub enum PageSrc {
    Local(PathBuf),
    Remote(PageImage),
}

/// Откуда брать страницы главы.
#[derive(Debug, Clone)]
pub enum Source {
    /// Глава скачана, страницы уже в сессии.
    Ready,
    /// Страницы надо запросить у сервера (`fetch_pages`).
    Remote,
    /// Прочитать главу нельзя: почему.
    Unavailable(&'static str),
}

/// Открытая глава и место в ней.
pub struct ReaderSession {
    pub start: ReaderStart,
    /// Глава в `start.chapters`.
    pub index: usize,
    pub pages: Vec<PageSrc>,
    /// Текущая страница с нуля.
    pub page: usize,
}

impl ReaderSession {
    pub fn new(start: ReaderStart) -> ReaderSession {
        ReaderSession {
            index: start.index,
            start,
            pages: Vec::new(),
            page: 0,
        }
    }

    pub fn chapter(&self) -> &Chapter {
        &self.start.chapters[self.index]
    }

    pub fn branch(&self) -> Option<u64> {
        self.start.branches.get(&self.chapter().id).copied().flatten()
    }

    /// Ключ главы в базе.
    pub fn key(&self) -> String {
        chapter_key(self.chapter().id, self.branch())
    }

    /// Id перевода текущей главы в выбранной ветке: его ждёт сайт.
    pub fn chapter_id(&self) -> u64 {
        let chapter = self.chapter();
        chapter.branch(self.branch()).map_or(chapter.id, |b| b.id)
    }

    pub fn has_prev(&self) -> bool {
        self.index > 0
    }

    pub fn has_next(&self) -> bool {
        self.index + 1 < self.start.chapters.len()
    }

    /// Открывает текущую главу: сбрасывает страницы и решает, откуда их брать.
    /// Второе значение – страница, на которой остановились в прошлый раз.
    pub fn open_chapter(&mut self, core: &Core) -> (Source, Option<usize>) {
        self.pages.clear();
        self.page = 0;
        let resume = core
            .db
            .progress(self.start.site, &self.start.slug, &self.key())
            .ok()
            .flatten()
            .filter(|p| !p.finished && p.position > 0)
            .map(|p| p.position as usize);
        if let Some(dir) = self.start.local.get(&self.chapter().id)
            && let Ok(files) = library::chapter_pages(dir)
            && !files.is_empty()
        {
            self.pages = files.into_iter().map(PageSrc::Local).collect();
            return (Source::Ready, resume);
        }
        if self.start.offline {
            return (Source::Unavailable("Эта глава не скачана, а сети нет"), resume);
        }
        if !self.chapter().branch(self.branch()).is_some_and(|b| b.is_open()) {
            return (Source::Unavailable("Глава закрыта: ранний доступ или платная"), resume);
        }
        (Source::Remote, resume)
    }

    /// Запрос страниц текущей главы у сервера. Возвращает future без ссылок на
    /// сессию, чтобы его можно было отдать в фоновую задачу.
    pub fn fetch_pages(&self, core: &Core) -> impl Future<Output = libsocial::Result<Vec<PageImage>>> + use<> {
        let client = core.client.clone();
        let (site, slug) = (self.start.site, self.start.slug.clone());
        let chapter = self.chapter().clone();
        let branch = self.branch();
        async move {
            client
                .chapter_pages(site, &slug, &chapter, branch)
                .await
                .map(|c| c.pages)
        }
    }

    /// Записывает место в главе. Когда глава дочитана впервые, на сайт уходит
    /// отметка о прочтении.
    pub fn save(&self, core: &Core) {
        if self.pages.is_empty() {
            return;
        }
        let finished = self.page + 1 >= self.pages.len();
        let (site, slug, key) = (self.start.site, &self.start.slug, self.key());
        let was_finished = core
            .db
            .progress(site, slug, &key)
            .ok()
            .flatten()
            .is_some_and(|p| p.finished);
        let _ = core.db.save_progress(
            site,
            slug,
            &key,
            Progress {
                position: self.page as u64,
                total: self.pages.len() as u64,
                finished,
            },
        );
        if finished && !was_finished {
            core.sync.push(Mark::ChapterViewed {
                site,
                title_id: self.start.title_id,
                chapter_id: self.chapter_id(),
            });
        }
    }

    /// Запоминает место в закладке на сайте. Зовётся при смене главы и при
    /// закрытии, а не на каждой странице.
    pub fn save_place(&self, core: &Core) {
        if self.pages.is_empty() {
            return;
        }
        core.sync.push(Mark::Place {
            site: self.start.site,
            slug: self.start.slug.clone(),
            place: Place {
                item_id: self.chapter_id(),
                item_number: self.chapter().item_number,
                progress: (self.page + 1).to_string(),
                player: None,
            },
        });
    }

    /// Переходит на соседнюю главу, записав место в текущей. `false`, если главы
    /// в эту сторону нет.
    pub fn go(&mut self, core: &Core, delta: isize) -> bool {
        let next = self.index as isize + delta;
        if next < 0 || next as usize >= self.start.chapters.len() {
            return false;
        }
        self.save(core);
        self.save_place(core);
        self.index = next as usize;
        true
    }

    /// Закрывает читалку: место записывается в базу и в закладку.
    pub fn close(&self, core: &Core) {
        self.save(core);
        self.save_place(core);
    }
}
