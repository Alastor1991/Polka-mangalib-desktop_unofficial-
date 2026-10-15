//! Каталог и поиск по выбранному сайту.

use iced::widget::{button, column, container, image, pick_list, row, scrollable, text, text_input};
use iced::{Alignment, ContentFit, Element, Fill, Task};
use libsocial::catalog::{CatalogFilter, CatalogSort};
use libsocial::models::TitleShort;
use libsocial::{Page, Site};
use polka_core::catalog::{Catalog as CatalogState, Ticket};

use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, human_error, style};

pub const CARD_W: f32 = 150.0;
pub const CARD_H: f32 = 212.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortChoice(pub CatalogSort);

impl std::fmt::Display for SortChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.label())
    }
}

pub struct Catalog {
    state: CatalogState,
    /// Строка поиска, пока её не отправили.
    text: String,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Site(Site),
    Text(String),
    Submit,
    Sort(SortChoice),
    Loaded(Ticket, Result<Page<TitleShort>, String>),
    Scrolled(scrollable::Viewport),
    More,
    Open(Site, String),
    Image(String, Loaded),
}

impl Catalog {
    pub fn new() -> Catalog {
        let filter = CatalogFilter {
            only_with_content: true,
            ..Default::default()
        };
        Catalog {
            state: CatalogState::new(Site::MangaLib, filter),
            text: String::new(),
            error: None,
        }
    }

    /// Загружает первую страницу заново или следующую.
    pub fn load(&mut self, ctx: &Ctx, next_page: bool) -> Task<Msg> {
        if !next_page {
            self.state.reload();
        }
        let Some(request) = self.state.next_request() else {
            return Task::none();
        };
        self.error = None;
        let ticket = request.ticket();
        let client = ctx.client.clone();
        Task::perform(
            async move { request.run(&client).await.map_err(|e| human_error(&e)) },
            move |result| Msg::Loaded(ticket, result),
        )
    }

    /// Меняет фильтр и грузит заново. Строка поиска берётся из поля, даже если её
    /// не отправили, как было и раньше: список всегда соответствует тому, что видно.
    fn set_filter(&mut self, ctx: &Ctx, change: impl FnOnce(&mut CatalogFilter)) -> Task<Msg> {
        let mut filter = self.state.filter().clone();
        filter.text = self.text.clone();
        change(&mut filter);
        self.state.set_filter(filter);
        self.load(ctx, false)
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> (Task<Msg>, Option<(Site, String)>) {
        let task = match msg {
            Msg::Site(site) => {
                self.state.set_site(site);
                self.set_filter(ctx, |_| {})
            }
            Msg::Text(text) => {
                self.text = text;
                Task::none()
            }
            Msg::Submit => self.set_filter(ctx, |_| {}),
            Msg::Sort(choice) => self.set_filter(ctx, |f| f.sort = choice.0),
            Msg::Loaded(ticket, result) => {
                let site = self.state.site();
                match self.state.apply(ticket, result) {
                    Ok(added) => {
                        let covers: Vec<Task<Msg>> = added
                            .iter()
                            .filter_map(|t| cover_req(site, t))
                            .map(|req| images.request(req, Msg::Image))
                            .collect();
                        Task::batch(covers)
                    }
                    Err(e) => {
                        self.error = Some(e);
                        Task::none()
                    }
                }
            }
            Msg::Scrolled(viewport) => {
                // После ошибки сами не повторяем: прокрутка приходит и при любом
                // изменении высоты списка, иначе запросы пошли бы по кругу.
                if self.error.is_none() && viewport.relative_offset().y > 0.92 {
                    self.load(ctx, true)
                } else {
                    Task::none()
                }
            }
            Msg::More => self.load(ctx, true),
            Msg::Open(site, slug) => return (Task::none(), Some((site, slug))),
            Msg::Image(key, loaded) => images.finish(key, loaded, Msg::Image),
        };
        (task, None)
    }

    pub fn view<'a>(&'a self, images: &'a Images) -> Element<'a, Msg> {
        // Ранобэ в окне iced не читается: его читалка умеет только картинки. Ядро
        // ранобэ знает, показывать его будет интерфейс на Slint.
        let sites = row(Site::ALL.into_iter().filter(|s| !s.is_ranobe()).map(|site| {
            button(text(site.name()).size(14))
                .padding([6, 12])
                .style(if site == self.state.site() {
                    button::primary
                } else {
                    button::secondary
                })
                .on_press(Msg::Site(site))
                .into()
        }))
        .spacing(6);

        let search = row![
            text_input("Поиск по названию", &self.text)
                .on_input(Msg::Text)
                .on_submit(Msg::Submit)
                .padding(8)
                .width(Fill),
            button("Найти").padding([8, 14]).on_press(Msg::Submit),
            pick_list(
                self.state.sorts().iter().copied().map(SortChoice).collect::<Vec<_>>(),
                Some(SortChoice(self.state.filter().sort)),
                Msg::Sort,
            )
            .padding(8),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let site = self.state.site();
        let cards = row(self.state.items().iter().map(|t| card(site, t, images)))
            .spacing(14)
            .wrap();

        let mut list = column![cards].spacing(16).padding(16).width(Fill);
        if let Some(error) = &self.error {
            list = list.push(
                row![
                    text(format!("Не загрузилось: {error}")),
                    button("Ещё раз").on_press(Msg::More)
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        } else if self.state.is_loading() {
            list = list.push(text("Загружаю…").style(text::secondary));
        } else if self.state.items().is_empty() {
            list = list.push(text("Ничего не нашлось.").style(text::secondary));
        } else if self.state.has_next() {
            list = list.push(button("Показать ещё").on_press(Msg::More));
        }

        column![
            column![sites, search].spacing(10).padding([12, 16]),
            scrollable(list).on_scroll(Msg::Scrolled).height(Fill),
        ]
        .into()
    }
}

pub fn cover_req(site: Site, t: &TitleShort) -> Option<ImageReq> {
    let url = t.cover.thumbnail.as_deref().or(t.cover.best())?;
    Some(ImageReq::Url {
        site,
        url: url.to_string(),
    })
}

/// Картинка обложки нужного размера или заглушка, пока она грузится.
pub fn cover<'a, M: 'a>(images: &'a Images, req: Option<&ImageReq>, w: f32, h: f32) -> Element<'a, M> {
    match req.and_then(|r| images.get(&r.key())) {
        Some(handle) => image(handle.clone())
            .width(w)
            .height(h)
            .content_fit(ContentFit::Cover)
            .into(),
        None => container(text("")).width(w).height(h).style(style::placeholder).into(),
    }
}

fn card<'a>(site: Site, t: &'a TitleShort, images: &'a Images) -> Element<'a, Msg> {
    let req = cover_req(site, t);
    let kind = t.kind.as_ref().map(|k| k.label.as_str()).unwrap_or("");
    let rating = t
        .rating
        .as_ref()
        .filter(|r| r.average > 0.0)
        .map(|r| format!("★ {:.1}", r.average))
        .unwrap_or_default();
    button(
        column![
            cover(images, req.as_ref(), CARD_W, CARD_H),
            text(t.display_name()).size(14).width(CARD_W).height(38),
            text(format!("{kind} {rating}").trim().to_string())
                .size(12)
                .style(text::secondary),
        ]
        .spacing(6),
    )
    .padding(6)
    .style(button::text)
    .on_press(Msg::Open(site, t.slug_url.clone()))
    .into()
}
