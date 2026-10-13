//! Библиотека. С входом это закладки аккаунта по папкам сайта («Читаю», «В планах»,
//! «Любимые» и свои), отдельно для манги и аниме. Раздел «На устройстве» показывает
//! тайтлы, добавленные без входа или со скачанным, и работает без сети.

use iced::widget::{button, column, row, scrollable, text};
use iced::{Alignment, Element, Fill, Task};
use libsocial::{Bookmark, BookmarkPage, Site};

use super::catalog::{CARD_H, CARD_W, cover};
use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, Message, human_error};
use crate::db::TitleRow;

/// Карточка тайтла в библиотеке.
struct Card {
    site: Site,
    slug: String,
    name: String,
    /// Где остановились, если сайт это знает.
    note: Option<String>,
    cover: Option<ImageReq>,
}

#[derive(Default)]
pub struct LibraryView {
    /// Раздел: `None` – «На устройстве», иначе папка закладок.
    folder: Option<u64>,
    /// Раздел выбран пользователем, а не подставлен по умолчанию.
    chosen: bool,
    cards: Vec<Card>,
    /// Следующая страница закладок по каждому сайту папки, если она есть.
    next: Vec<(Site, u32)>,
    loading: bool,
    error: Option<String>,
    /// Номер загрузки. Ответы от прежних загрузок отбрасываются, иначе после
    /// повторного открытия папки карточки задвоились бы.
    generation: u64,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Open(Site, String),
    Image(String, Loaded),
    Folder(Option<u64>),
    More,
    /// Страницы закладок по сайтам для загрузки с этим номером: сайт, номер
    /// страницы, ответ.
    Loaded(u64, Vec<(Site, u32, Result<BookmarkPage, String>)>),
}

impl LibraryView {
    pub fn reload(&mut self, ctx: &Ctx, images: &mut Images) -> Task<Message> {
        // С входом по умолчанию открываются закладки, без него – устройство.
        if !self.chosen {
            self.folder = ctx.folders.first().map(|f| f.id).filter(|_| ctx.user_id.is_some());
        }
        if self
            .folder
            .is_some_and(|id| ctx.user_id.is_none() || !ctx.folders.iter().any(|f| f.id == id))
        {
            self.folder = None;
        }
        self.cards.clear();
        self.next.clear();
        self.error = None;
        self.generation += 1;
        match (self.folder, ctx.user_id) {
            (Some(folder), Some(user_id)) => {
                let sites = folder_sites(ctx, folder);
                self.load(ctx, user_id, folder, sites.into_iter().map(|s| (s, 1)).collect())
            }
            _ => self.load_local(ctx, images),
        }
    }

    fn load_local(&mut self, ctx: &Ctx, images: &mut Images) -> Task<Message> {
        self.loading = false;
        match ctx.db.library() {
            Ok(rows) => self.cards = rows.iter().map(|r| local_card(ctx, r)).collect(),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.request_covers(images)
    }

    fn load(&mut self, ctx: &Ctx, user_id: u64, folder: u64, pages: Vec<(Site, u32)>) -> Task<Message> {
        self.loading = true;
        let generation = self.generation;
        let client = ctx.client.clone();
        Task::perform(
            async move {
                let mut out = Vec::new();
                // По сайтам по очереди: запросы и так идут через общую очередь API.
                for (site, page) in pages {
                    let result = client
                        .bookmarks(site, user_id, folder, page)
                        .await
                        .map_err(|e| human_error(&e));
                    out.push((site, page, result));
                }
                out
            },
            move |r| Message::Library(Msg::Loaded(generation, r)),
        )
    }

    fn request_covers(&self, images: &mut Images) -> Task<Message> {
        let tasks: Vec<_> = self
            .cards
            .iter()
            .filter_map(|c| c.cover.clone())
            .map(|req| images.request(req, |k, l| Message::Library(Msg::Image(k, l))))
            .collect();
        Task::batch(tasks)
    }

    /// Возвращает тайтл, который надо открыть, или задачу.
    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> (Task<Message>, Option<(Site, String)>) {
        match msg {
            Msg::Open(site, slug) => (Task::none(), Some((site, slug))),
            Msg::Image(key, loaded) => (
                images.finish(key, loaded, |k, l| Message::Library(Msg::Image(k, l))),
                None,
            ),
            Msg::Folder(folder) => {
                self.folder = folder;
                self.chosen = true;
                (self.reload(ctx, images), None)
            }
            Msg::More => {
                let (Some(folder), Some(user_id)) = (self.folder, ctx.user_id) else {
                    return (Task::none(), None);
                };
                if self.loading || self.next.is_empty() {
                    return (Task::none(), None);
                }
                let pages = std::mem::take(&mut self.next);
                (self.load(ctx, user_id, folder, pages), None)
            }
            // Ответ прежней загрузки: папку успели сменить или перечитать.
            Msg::Loaded(generation, _) if generation != self.generation => (Task::none(), None),
            Msg::Loaded(_, results) => {
                self.loading = false;
                let mut errors = Vec::new();
                for (site, page, result) in results {
                    match result {
                        Ok(found) => {
                            if found.has_next {
                                self.next.push((site, page + 1));
                            }
                            self.cards
                                .extend(found.bookmarks.iter().filter_map(|b| remote_card(site, b)));
                        }
                        Err(e) => errors.push(format!("{}: {e}", site.name())),
                    }
                }
                self.error = (!errors.is_empty()).then(|| errors.join("; "));
                (self.request_covers(images), None)
            }
        }
    }

    pub fn view<'a>(&'a self, ctx: &'a Ctx, images: &'a Images) -> Element<'a, Msg> {
        let mut page = column![self.sections(ctx)].spacing(14).padding(16).width(Fill);
        if let Some(e) = &self.error {
            page = page.push(text(format!("Не загрузилось: {e}")).style(text::danger));
        }
        if self.cards.is_empty() && !self.loading {
            let hint = if self.folder.is_some() {
                "В этой папке пусто."
            } else {
                "Здесь появятся тайтлы, которые вы добавите в библиотеку или начнёте скачивать."
            };
            page = page.push(text(hint).style(text::secondary));
        }
        let cards = row(self.cards.iter().map(|c| {
            let mut card = column![
                cover(images, c.cover.as_ref(), CARD_W, CARD_H),
                text(&c.name).size(14).width(CARD_W).height(38),
                text(c.site.name()).size(12).style(text::secondary),
            ]
            .spacing(6);
            if let Some(note) = &c.note {
                card = card.push(text(note.as_str()).size(12).style(text::secondary));
            }
            button(card)
                .padding(6)
                .style(button::text)
                .on_press(Msg::Open(c.site, c.slug.clone()))
                .into()
        }))
        .spacing(14)
        .wrap();
        page = page.push(cards);
        if self.loading {
            page = page.push(text("Загружаю…").style(text::secondary));
        } else if !self.next.is_empty() {
            page = page.push(button("Показать ещё").style(button::secondary).on_press(Msg::More));
        }
        scrollable(page).height(Fill).into()
    }

    /// Переключатель разделов: устройство и папки сайта, манга и аниме отдельно,
    /// потому что названия папок у них повторяются.
    fn sections<'a>(&self, ctx: &'a Ctx) -> Element<'a, Msg> {
        let tab = |label: String, folder: Option<u64>| {
            button(text(label).size(14))
                .padding([6, 12])
                .style(if self.folder == folder {
                    button::primary
                } else {
                    button::text
                })
                .on_press(Msg::Folder(folder))
        };
        let mut rows = column![row![tab("На устройстве".into(), None)]].spacing(4);
        if ctx.user_id.is_some() {
            for (label, anime) in [("Манга:", false), ("Аниме:", true)] {
                let folders: Vec<_> = ctx
                    .folders
                    .iter()
                    .filter(|f| f.is_for(Site::AnimeLib) == anime)
                    .collect();
                if folders.is_empty() {
                    continue;
                }
                let mut line = row![text(label).size(13).style(text::secondary).width(60)]
                    .spacing(4)
                    .align_y(Alignment::Center);
                for f in folders {
                    line = line.push(tab(f.name.clone(), Some(f.id)));
                }
                rows = rows.push(line.wrap());
            }
        }
        rows.into()
    }
}

/// Сайты, на которых есть эта папка: у манги их три, у аниме один.
fn folder_sites(ctx: &Ctx, folder: u64) -> Vec<Site> {
    let Some(f) = ctx.folders.iter().find(|f| f.id == folder) else {
        return Vec::new();
    };
    Site::ALL.into_iter().filter(|s| f.is_for(*s)).collect()
}

fn local_card(ctx: &Ctx, r: &TitleRow) -> Card {
    // Обложка с диска, если тайтл скачивался, иначе из сети.
    let local = ctx.library.cover_path(r.site, &r.slug);
    let cover = if local.is_file() {
        Some(ImageReq::File(local))
    } else {
        r.cover_url.as_ref().map(|url| ImageReq::Url {
            site: r.site,
            url: url.clone(),
        })
    };
    Card {
        site: r.site,
        slug: r.slug.clone(),
        name: r.name.clone(),
        note: None,
        cover,
    }
}

fn remote_card(site: Site, b: &Bookmark) -> Option<Card> {
    let media = b.media.as_ref()?;
    let note = b.item.as_ref().map(|item| {
        if site.is_anime() {
            format!("{} серия", item.number)
        } else {
            format!("Том {} Глава {}", item.volume, item.number)
        }
    });
    Some(Card {
        site,
        slug: media.slug_url.clone(),
        name: media.display_name().to_string(),
        note,
        cover: media.cover.best().map(|url| ImageReq::Url {
            site,
            url: url.to_string(),
        }),
    })
}
