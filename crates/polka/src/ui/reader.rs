//! Читалка: лента для вебтунов и постраничный режим, с диска или из сети.

use std::collections::HashMap;
use std::path::PathBuf;

use iced::keyboard::{self, Key, key::Named};
use iced::widget::operation::{AbsoluteOffset, scroll_by, snap_to};
use iced::widget::{button, column, container, image, row, scrollable, text};
use iced::{Alignment, ContentFit, Element, Fill, Subscription, Task};
use libsocial::Site;
use libsocial::models::{Chapter, PageImage};

use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, human_error, style};
use crate::db::Progress;
use crate::library::{self, chapter_key};

const SCROLL_ID: &str = "reader-scroll";
/// Сколько страниц вперёд грузить заранее.
const AHEAD: usize = 6;

/// С чего открыть читалку. Главы идут от первой к последней.
#[derive(Debug, Clone)]
pub struct ReaderStart {
    pub site: Site,
    pub slug: String,
    pub title: String,
    pub chapters: Vec<Chapter>,
    pub branches: HashMap<u64, Option<u64>>,
    pub local: HashMap<u64, PathBuf>,
    pub index: usize,
    pub offline: bool,
}

#[derive(Debug, Clone)]
enum PageSrc {
    Local(PathBuf),
    Remote(PageImage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ribbon,
    Paged,
}

pub struct Reader {
    start: ReaderStart,
    index: usize,
    pages: Vec<PageSrc>,
    loading: bool,
    error: Option<String>,
    mode: Mode,
    page: usize,
    saved_page: Option<usize>,
    server: String,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Pages(u64, Result<Vec<PageImage>, String>),
    Scrolled(scrollable::Viewport),
    NextChapter,
    PrevChapter,
    NextPage,
    PrevPage,
    ScrollDown,
    ScrollUp,
    Mode,
    Close,
    Retry,
    Image(String, Loaded),
}

pub enum Effect {
    Task(Task<Msg>),
    Close,
}

impl Reader {
    pub fn open(start: ReaderStart, ctx: &Ctx, images: &mut Images) -> (Reader, Task<Msg>) {
        let index = start.index;
        let mut reader = Reader {
            start,
            index,
            pages: Vec::new(),
            loading: false,
            error: None,
            mode: Mode::Ribbon,
            page: 0,
            saved_page: None,
            server: ctx.config.image_server.clone(),
        };
        let task = reader.load_chapter(ctx, images);
        (reader, task)
    }

    fn chapter(&self) -> &Chapter {
        &self.start.chapters[self.index]
    }

    fn key(&self) -> String {
        let c = self.chapter();
        chapter_key(c.id, self.start.branches.get(&c.id).copied().flatten())
    }

    fn load_chapter(&mut self, ctx: &Ctx, images: &mut Images) -> Task<Msg> {
        images.forget_pages();
        self.pages.clear();
        self.error = None;
        self.page = 0;
        self.saved_page = None;
        let chapter = self.chapter().clone();
        let resume = ctx
            .db
            .progress(self.start.site, &self.start.slug, &self.key())
            .ok()
            .flatten()
            .filter(|p| !p.finished && p.position > 0)
            .map(|p| p.position as usize);

        if let Some(dir) = self.start.local.get(&chapter.id) {
            match library::chapter_pages(dir) {
                Ok(files) if !files.is_empty() => {
                    self.pages = files.into_iter().map(PageSrc::Local).collect();
                    self.loading = false;
                    return Task::batch([self.ensure_window(images), self.jump_to(resume)]);
                }
                _ => {}
            }
        }
        if self.start.offline {
            self.error = Some("Эта глава не скачана, а сети нет".into());
            return Task::none();
        }
        let branch = self.start.branches.get(&chapter.id).copied().flatten();
        let branch_ok = chapter.branch(branch).is_some_and(|b| b.is_open());
        if !branch_ok {
            self.error = Some("Глава закрыта: ранний доступ или платная".into());
            return Task::none();
        }
        self.loading = true;
        self.saved_page = resume;
        let client = ctx.client.clone();
        let site = self.start.site;
        let slug = self.start.slug.clone();
        let id = chapter.id;
        Task::perform(
            async move {
                client
                    .chapter_pages(site, &slug, &chapter, branch)
                    .await
                    .map(|c| c.pages)
                    .map_err(|e| human_error(&e))
            },
            move |r| Msg::Pages(id, r),
        )
    }

    /// Перематывает к сохранённой странице. В ленте позиция приблизительная:
    /// высоты страниц до загрузки неизвестны.
    fn jump_to(&mut self, page: Option<usize>) -> Task<Msg> {
        let Some(page) = page.filter(|p| *p < self.pages.len()) else {
            return snap_to(SCROLL_ID, scrollable::RelativeOffset::START);
        };
        self.page = page;
        let y = page as f32 / self.pages.len().max(1) as f32;
        snap_to(SCROLL_ID, scrollable::RelativeOffset { x: 0.0, y })
    }

    fn req(&self, i: usize) -> Option<ImageReq> {
        Some(match self.pages.get(i)? {
            PageSrc::Local(path) => ImageReq::LocalPage(path.clone()),
            PageSrc::Remote(page) => ImageReq::Page {
                site: self.start.site,
                page: page.clone(),
                server: self.server.clone(),
            },
        })
    }

    /// Запрашивает страницы вокруг текущей.
    pub fn ensure_window(&mut self, images: &mut Images) -> Task<Msg> {
        let from = self.page.saturating_sub(1);
        let to = (self.page + AHEAD).min(self.pages.len());
        let tasks: Vec<_> = (from..to)
            .filter_map(|i| self.req(i))
            .map(|r| images.request(r, Msg::Image))
            .collect();
        Task::batch(tasks)
    }

    fn save(&self, ctx: &Ctx) {
        if self.pages.is_empty() {
            return;
        }
        let total = self.pages.len() as u64;
        let finished = self.page + 1 >= self.pages.len();
        let _ = ctx.db.save_progress(
            self.start.site,
            &self.start.slug,
            &self.key(),
            Progress {
                position: self.page as u64,
                total,
                finished,
            },
        );
    }

    fn go_chapter(&mut self, delta: isize, ctx: &Ctx, images: &mut Images) -> Task<Msg> {
        let next = self.index as isize + delta;
        if next < 0 || next as usize >= self.start.chapters.len() {
            return Task::none();
        }
        self.save(ctx);
        self.index = next as usize;
        self.load_chapter(ctx, images)
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> Effect {
        let task = match msg {
            Msg::Pages(id, result) => {
                if id != self.chapter().id {
                    return Effect::Task(Task::none());
                }
                self.loading = false;
                match result {
                    Ok(pages) if !pages.is_empty() => {
                        self.pages = pages.into_iter().map(PageSrc::Remote).collect();
                        let resume = self.saved_page.take();
                        Task::batch([self.ensure_window(images), self.jump_to(resume)])
                    }
                    Ok(_) => {
                        self.error = Some("В главе нет страниц".into());
                        Task::none()
                    }
                    Err(e) => {
                        self.error = Some(e);
                        Task::none()
                    }
                }
            }
            Msg::Scrolled(viewport) => {
                if self.pages.is_empty() {
                    return Effect::Task(Task::none());
                }
                let y = viewport.relative_offset().y.clamp(0.0, 1.0);
                let page = if y >= 0.995 {
                    self.pages.len() - 1
                } else {
                    ((y * self.pages.len() as f32) as usize).min(self.pages.len() - 1)
                };
                if page != self.page {
                    self.page = page;
                    self.save(ctx);
                    self.ensure_window(images)
                } else {
                    Task::none()
                }
            }
            Msg::NextPage => {
                if self.mode == Mode::Paged {
                    if self.page + 1 < self.pages.len() {
                        self.page += 1;
                        self.save(ctx);
                        self.ensure_window(images)
                    } else {
                        self.go_chapter(1, ctx, images)
                    }
                } else {
                    scroll_by(SCROLL_ID, AbsoluteOffset { x: 0.0, y: 700.0 })
                }
            }
            Msg::PrevPage => {
                if self.mode == Mode::Paged {
                    if self.page > 0 {
                        self.page -= 1;
                        self.save(ctx);
                        self.ensure_window(images)
                    } else {
                        Task::none()
                    }
                } else {
                    scroll_by(SCROLL_ID, AbsoluteOffset { x: 0.0, y: -700.0 })
                }
            }
            Msg::ScrollDown => scroll_by(SCROLL_ID, AbsoluteOffset { x: 0.0, y: 160.0 }),
            Msg::ScrollUp => scroll_by(SCROLL_ID, AbsoluteOffset { x: 0.0, y: -160.0 }),
            Msg::NextChapter => self.go_chapter(1, ctx, images),
            Msg::PrevChapter => self.go_chapter(-1, ctx, images),
            Msg::Mode => {
                self.mode = match self.mode {
                    Mode::Ribbon => Mode::Paged,
                    Mode::Paged => Mode::Ribbon,
                };
                let page = Some(self.page);
                Task::batch([self.ensure_window(images), self.jump_to(page)])
            }
            Msg::Close => {
                self.save(ctx);
                return Effect::Close;
            }
            Msg::Retry => {
                let failed: Vec<ImageReq> = (0..self.pages.len())
                    .filter_map(|i| self.req(i))
                    .filter(|r| images.error(&r.key()).is_some())
                    .collect();
                let tasks: Vec<_> = failed.into_iter().map(|r| images.retry(r, Msg::Image)).collect();
                if self.pages.is_empty() {
                    self.load_chapter(ctx, images)
                } else {
                    Task::batch(tasks)
                }
            }
            // Результаты картинок принимает приложение целиком, см. App::update.
            Msg::Image(..) => Task::none(),
        };
        Effect::Task(task)
    }

    pub fn view<'a>(&'a self, images: &'a Images) -> Element<'a, Msg> {
        let chapter = self.chapter();
        let has_prev = self.index > 0;
        let has_next = self.index + 1 < self.start.chapters.len();
        let counter = if self.pages.is_empty() {
            String::new()
        } else {
            format!("{} / {}", self.page + 1, self.pages.len())
        };
        let bar = container(
            row![
                button("← К тайтлу").style(button::text).on_press(Msg::Close),
                column![text(&self.start.title).size(13), text(chapter.title()).size(16)].width(Fill),
                text(counter).size(14),
                button("‹ Глава")
                    .style(button::secondary)
                    .on_press_maybe(has_prev.then_some(Msg::PrevChapter)),
                button("Глава ›")
                    .style(button::secondary)
                    .on_press_maybe(has_next.then_some(Msg::NextChapter)),
                button(match self.mode {
                    Mode::Ribbon => "Режим: лента",
                    Mode::Paged => "Режим: страницы",
                })
                .style(button::text)
                .on_press(Msg::Mode),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .style(style::bar);

        let body: Element<'a, Msg> = if let Some(error) = &self.error {
            container(
                column![
                    text(error.as_str()).size(16),
                    row![
                        button("Ещё раз").on_press(Msg::Retry),
                        button("Следующая глава")
                            .style(button::secondary)
                            .on_press_maybe(has_next.then_some(Msg::NextChapter)),
                    ]
                    .spacing(10)
                ]
                .spacing(12)
                .align_x(Alignment::Center),
            )
            .center_x(Fill)
            .center_y(Fill)
            .into()
        } else if self.loading {
            container(text("Загружаю главу…")).center_x(Fill).center_y(Fill).into()
        } else {
            match self.mode {
                Mode::Ribbon => self.ribbon(images, has_next),
                Mode::Paged => self.paged(images),
            }
        };

        container(column![bar, body])
            .style(style::reader_bg)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn page_view<'a>(&'a self, i: usize, images: &'a Images, fill_height: bool) -> Element<'a, Msg> {
        let Some(req) = self.req(i) else { return text("").into() };
        let key = req.key();
        if let Some(handle) = images.get(&key) {
            let img = image(handle.clone()).content_fit(ContentFit::Contain);
            return if fill_height {
                img.width(Fill).height(Fill).into()
            } else {
                img.width(Fill).into()
            };
        }
        let height = match &self.pages[i] {
            PageSrc::Remote(p) => match (p.width, p.height) {
                (Some(w), Some(h)) if w > 0 => (900.0 * h as f32 / w as f32).min(4000.0),
                _ => 1200.0,
            },
            PageSrc::Local(_) => 1200.0,
        };
        let label = match images.error(&key) {
            Some(e) => format!("Страница {} не загрузилась: {e}", i + 1),
            None => format!("Страница {}…", i + 1),
        };
        let mut placeholder = column![text(label).size(14)].spacing(8).align_x(Alignment::Center);
        if images.error(&key).is_some() {
            placeholder = placeholder.push(button("Ещё раз").on_press(Msg::Retry));
        }
        container(placeholder)
            .center_x(Fill)
            .center_y(if fill_height { Fill } else { iced::Length::Fixed(height) })
            .into()
    }

    fn ribbon<'a>(&'a self, images: &'a Images, has_next: bool) -> Element<'a, Msg> {
        let pages = column((0..self.pages.len()).map(|i| self.page_view(i, images, false)))
            .width(Fill)
            .max_width(900);
        let tail = container(
            button(if has_next {
                "Следующая глава ›"
            } else {
                "Это последняя глава"
            })
            .padding([10, 20])
            .on_press_maybe(has_next.then_some(Msg::NextChapter)),
        )
        .padding(30)
        .center_x(Fill);
        scrollable(column![container(pages).center_x(Fill), tail])
            .id(SCROLL_ID)
            .on_scroll(Msg::Scrolled)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn paged<'a>(&'a self, images: &'a Images) -> Element<'a, Msg> {
        row![
            button(container(text("‹").size(28)).center_y(Fill))
                .style(button::text)
                .height(Fill)
                .on_press(Msg::PrevPage),
            container(self.page_view(self.page, images, true))
                .width(Fill)
                .height(Fill),
            button(container(text("›").size(28)).center_y(Fill))
                .style(button::text)
                .height(Fill)
                .on_press(Msg::NextPage),
        ]
        .into()
    }
}

/// Клавиши читалки.
pub fn keys() -> Subscription<Msg> {
    keyboard::listen().filter_map(key_to_msg)
}

fn key_to_msg(event: keyboard::Event) -> Option<Msg> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };
    match key.as_ref() {
        Key::Named(Named::ArrowRight) | Key::Named(Named::PageDown) => Some(Msg::NextPage),
        Key::Named(Named::Space) if modifiers.shift() => Some(Msg::PrevPage),
        Key::Named(Named::Space) => Some(Msg::NextPage),
        Key::Named(Named::ArrowLeft) | Key::Named(Named::PageUp) => Some(Msg::PrevPage),
        Key::Named(Named::ArrowDown) => Some(Msg::ScrollDown),
        Key::Named(Named::ArrowUp) => Some(Msg::ScrollUp),
        Key::Named(Named::Escape) => Some(Msg::Close),
        Key::Character("]") | Key::Character("n") => Some(Msg::NextChapter),
        Key::Character("[") | Key::Character("p") => Some(Msg::PrevChapter),
        Key::Character("m") => Some(Msg::Mode),
        _ => None,
    }
}
