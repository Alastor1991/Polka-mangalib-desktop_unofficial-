//! Читалка: лента для вебтунов и постраничный режим, с диска или из сети. Что
//! открыть, откуда брать страницы и что записать, решает ядро (`ReaderSession`),
//! здесь только отрисовка, прокрутка и картинки.

use iced::keyboard::{self, Key, key::Named};
use iced::widget::operation::{AbsoluteOffset, scroll_by, snap_to};
use iced::widget::{button, column, container, image, row, scrollable, text};
use iced::{Alignment, ContentFit, Element, Fill, Subscription, Task};
use libsocial::models::PageImage;
use polka_core::reader::{PageSrc, ReaderSession, ReaderStart, Source};

use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, human_error, style};

const SCROLL_ID: &str = "reader-scroll";
/// Сколько страниц вперёд грузить заранее.
const AHEAD: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ribbon,
    Paged,
}

pub struct Reader {
    s: ReaderSession,
    loading: bool,
    error: Option<String>,
    mode: Mode,
    /// Куда перемотать, когда придут страницы из сети.
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
        let mut reader = Reader {
            s: ReaderSession::new(start),
            loading: false,
            error: None,
            mode: Mode::Ribbon,
            saved_page: None,
            server: ctx.config.image_server.clone(),
        };
        let task = reader.load_chapter(ctx, images);
        (reader, task)
    }

    fn load_chapter(&mut self, ctx: &Ctx, images: &mut Images) -> Task<Msg> {
        images.forget_pages();
        self.error = None;
        self.saved_page = None;
        self.loading = false;
        let (source, resume) = self.s.open_chapter(ctx);
        match source {
            Source::Ready => Task::batch([self.ensure_window(images), self.jump_to(resume)]),
            Source::Unavailable(why) => {
                self.error = Some(why.into());
                Task::none()
            }
            Source::Remote => {
                self.loading = true;
                self.saved_page = resume;
                let id = self.s.chapter().id;
                let fetch = self.s.fetch_pages(ctx);
                Task::perform(async move { fetch.await.map_err(|e| human_error(&e)) }, move |r| {
                    Msg::Pages(id, r)
                })
            }
        }
    }

    /// Перематывает к сохранённой странице. В ленте позиция приблизительная:
    /// высоты страниц до загрузки неизвестны.
    fn jump_to(&mut self, page: Option<usize>) -> Task<Msg> {
        let Some(page) = page.filter(|p| *p < self.s.pages.len()) else {
            return snap_to(SCROLL_ID, scrollable::RelativeOffset::START);
        };
        self.s.page = page;
        let y = page as f32 / self.s.pages.len().max(1) as f32;
        snap_to(SCROLL_ID, scrollable::RelativeOffset { x: 0.0, y })
    }

    fn req(&self, i: usize) -> Option<ImageReq> {
        Some(match self.s.pages.get(i)? {
            PageSrc::Local(path) => ImageReq::LocalPage(path.clone()),
            PageSrc::Remote(page) => ImageReq::Page {
                site: self.s.start.site,
                page: page.clone(),
                server: self.server.clone(),
            },
        })
    }

    /// Запрашивает страницы вокруг текущей.
    pub fn ensure_window(&mut self, images: &mut Images) -> Task<Msg> {
        let from = self.s.page.saturating_sub(1);
        let to = (self.s.page + AHEAD).min(self.s.pages.len());
        let tasks: Vec<_> = (from..to)
            .filter_map(|i| self.req(i))
            .map(|r| images.request(r, Msg::Image))
            .collect();
        Task::batch(tasks)
    }

    fn go_chapter(&mut self, delta: isize, ctx: &Ctx, images: &mut Images) -> Task<Msg> {
        if self.s.go(ctx, delta) {
            self.load_chapter(ctx, images)
        } else {
            Task::none()
        }
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> Effect {
        let task = match msg {
            Msg::Pages(id, result) => {
                if id != self.s.chapter().id {
                    return Effect::Task(Task::none());
                }
                self.loading = false;
                match result {
                    Ok(pages) if !pages.is_empty() => {
                        self.s.pages = pages.into_iter().map(PageSrc::Remote).collect();
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
                if self.s.pages.is_empty() {
                    return Effect::Task(Task::none());
                }
                let y = viewport.relative_offset().y.clamp(0.0, 1.0);
                let page = if y >= 0.995 {
                    self.s.pages.len() - 1
                } else {
                    ((y * self.s.pages.len() as f32) as usize).min(self.s.pages.len() - 1)
                };
                if page != self.s.page {
                    self.s.page = page;
                    self.s.save(ctx);
                    self.ensure_window(images)
                } else {
                    Task::none()
                }
            }
            Msg::NextPage => {
                if self.mode == Mode::Paged {
                    if self.s.page + 1 < self.s.pages.len() {
                        self.s.page += 1;
                        self.s.save(ctx);
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
                    if self.s.page > 0 {
                        self.s.page -= 1;
                        self.s.save(ctx);
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
                let page = Some(self.s.page);
                Task::batch([self.ensure_window(images), self.jump_to(page)])
            }
            Msg::Close => {
                self.s.close(ctx);
                return Effect::Close;
            }
            Msg::Retry => {
                let failed: Vec<ImageReq> = (0..self.s.pages.len())
                    .filter_map(|i| self.req(i))
                    .filter(|r| images.error(&r.key()).is_some())
                    .collect();
                let tasks: Vec<_> = failed.into_iter().map(|r| images.retry(r, Msg::Image)).collect();
                if self.s.pages.is_empty() {
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
        let chapter = self.s.chapter();
        let has_prev = self.s.has_prev();
        let has_next = self.s.has_next();
        let counter = if self.s.pages.is_empty() {
            String::new()
        } else {
            format!("{} / {}", self.s.page + 1, self.s.pages.len())
        };
        let bar = container(
            row![
                button("← К тайтлу").style(button::text).on_press(Msg::Close),
                column![text(&self.s.start.title).size(13), text(chapter.title()).size(16)].width(Fill),
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
        let height = match &self.s.pages[i] {
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
        let pages = column((0..self.s.pages.len()).map(|i| self.page_view(i, images, false)))
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
            container(self.page_view(self.s.page, images, true))
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
