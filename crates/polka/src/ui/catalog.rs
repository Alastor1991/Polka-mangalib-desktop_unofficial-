//! Каталог и поиск по выбранному сайту.

use iced::widget::{button, column, container, image, pick_list, row, scrollable, text, text_input};
use iced::{Alignment, ContentFit, Element, Fill, Task};
use libsocial::models::{Paged, TitleShort};
use libsocial::{CatalogQuery, Site, Sort};

use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, human_error, style};

pub const CARD_W: f32 = 150.0;
pub const CARD_H: f32 = 212.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortChoice(pub Sort);

impl SortChoice {
    const ALL: [SortChoice; 6] = [
        SortChoice(Sort::Popular),
        SortChoice(Sort::Rating),
        SortChoice(Sort::Views),
        SortChoice(Sort::LastUpdate),
        SortChoice(Sort::Newest),
        SortChoice(Sort::Name),
    ];
}

impl std::fmt::Display for SortChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Sort::Popular => "Популярное",
            Sort::Rating => "По рейтингу",
            Sort::Views => "По просмотрам",
            Sort::LastUpdate => "Обновлённые",
            Sort::Newest => "Новые на сайте",
            Sort::Name => "По названию",
        })
    }
}

pub struct Catalog {
    site: Site,
    text: String,
    sort: Sort,
    items: Vec<TitleShort>,
    page: u32,
    has_next: bool,
    loading: bool,
    error: Option<String>,
    /// Номер запроса: ответ на устаревший запрос (сменили сайт, пока грузилось) выбрасывается.
    request: u64,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Site(Site),
    Text(String),
    Submit,
    Sort(SortChoice),
    Loaded(u64, Result<Paged<TitleShort>, String>),
    Scrolled(scrollable::Viewport),
    More,
    Open(Site, String),
    Image(String, Loaded),
}

impl Catalog {
    pub fn new() -> Catalog {
        Catalog {
            site: Site::MangaLib,
            text: String::new(),
            sort: Sort::Popular,
            items: Vec::new(),
            page: 0,
            has_next: true,
            loading: false,
            error: None,
            request: 0,
        }
    }

    /// Загружает первую страницу заново или следующую.
    pub fn load(&mut self, ctx: &Ctx, next_page: bool) -> Task<Msg> {
        if next_page && (self.loading || !self.has_next) {
            return Task::none();
        }
        if !next_page {
            self.items.clear();
            self.page = 0;
            self.has_next = true;
        }
        self.loading = true;
        self.error = None;
        self.request += 1;
        let request = self.request;
        let query = CatalogQuery {
            text: self.text.clone(),
            page: self.page + 1,
            sort: self.sort,
            with_content: true,
            ..Default::default()
        };
        let client = ctx.client.clone();
        let site = self.site;
        Task::perform(
            async move { client.catalog(site, &query).await.map_err(|e| human_error(&e)) },
            move |result| Msg::Loaded(request, result),
        )
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> (Task<Msg>, Option<(Site, String)>) {
        let task = match msg {
            Msg::Site(site) => {
                self.site = site;
                self.load(ctx, false)
            }
            Msg::Text(text) => {
                self.text = text;
                Task::none()
            }
            Msg::Submit => self.load(ctx, false),
            Msg::Sort(choice) => {
                self.sort = choice.0;
                self.load(ctx, false)
            }
            Msg::Loaded(request, result) => {
                if request != self.request {
                    return (Task::none(), None);
                }
                self.loading = false;
                match result {
                    Ok(page) => {
                        self.page += 1;
                        self.has_next = page.meta.has_next_page;
                        let covers: Vec<Task<Msg>> = page
                            .data
                            .iter()
                            .filter_map(|t| cover_req(self.site, t))
                            .map(|req| images.request(req, Msg::Image))
                            .collect();
                        // Один и тот же тайтл иногда приходит на двух страницах подряд.
                        for item in page.data {
                            if !self.items.iter().any(|i| i.id == item.id) {
                                self.items.push(item);
                            }
                        }
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
                .style(if site == self.site {
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
            pick_list(SortChoice::ALL, Some(SortChoice(self.sort)), Msg::Sort).padding(8),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let cards = row(self.items.iter().map(|t| card(self.site, t, images)))
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
        } else if self.loading {
            list = list.push(text("Загружаю…").style(text::secondary));
        } else if self.items.is_empty() {
            list = list.push(text("Ничего не нашлось.").style(text::secondary));
        } else if self.has_next {
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
