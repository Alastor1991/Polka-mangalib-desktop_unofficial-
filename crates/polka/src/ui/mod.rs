//! Интерфейс на iced: каталог, тайтл, читалка, библиотека, загрузки, настройки.

mod catalog;
mod downloads_view;
mod images;
mod library_view;
mod reader;
mod settings;
mod style;
mod title;

use std::sync::OnceLock;

use iced::widget::{button, column, container, row, text};
use iced::{Element, Fill, Subscription, Task, Theme};
use libsocial::models::Me;
use libsocial::{Client, Site};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::db::Db;
use crate::downloads::{DownloadEvent, Downloader};
use crate::library::Library;
use crate::secrets::TokenStore;

/// Всё, что нужно экранам для работы. Клонируется дёшево.
#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub db: Db,
    pub library: Library,
    pub downloader: Downloader,
    pub config: Config,
    pub store: TokenStore,
}

/// Канал событий загрузчика для подписки. Подписке в iced нельзя передать замыкание
/// с данными, поэтому отправитель лежит в глобальной ячейке.
static DOWNLOAD_EVENTS: OnceLock<broadcast::Sender<DownloadEvent>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Catalog,
    Library,
    Downloads,
    Settings,
}

pub struct App {
    ctx: Ctx,
    tab: Tab,
    catalog: catalog::Catalog,
    title: Option<title::TitleView>,
    reader: Option<reader::Reader>,
    library: library_view::LibraryView,
    downloads: downloads_view::DownloadsView,
    settings: settings::Settings,
    images: images::Images,
    me: Option<Me>,
    toast: Option<String>,
    /// Отладка: открыть главу с этим номером в списке, как только тайтл загрузится.
    dev_read: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tab(Tab),
    Catalog(catalog::Msg),
    Title(title::Msg),
    Reader(reader::Msg),
    Library(library_view::Msg),
    Downloads(downloads_view::Msg),
    Settings(settings::Msg),
    DownloadEvent(DownloadEvent),
    Me(Option<Me>),
    DismissToast,
}

pub fn run(ctx: Ctx) -> iced::Result {
    let _ = DOWNLOAD_EVENTS.set(ctx.downloader.event_sender());
    let cache_limit = ctx.config.cache_limit_mb * 1024 * 1024;
    let cache = crate::config::cache_dir().join("images");
    std::thread::spawn(move || {
        let _ = images::prune_cache(&cache, cache_limit);
    });
    iced::application(move || App::new(ctx.clone()), App::update, App::view)
        .title("Полка")
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size((1180.0, 820.0))
        .run()
}

impl App {
    fn new(ctx: Ctx) -> (App, Task<Message>) {
        let images = images::Images::new(
            ctx.client.clone(),
            crate::config::cache_dir().join("images"),
            ctx.config.download_concurrency,
        );
        let mut app = App {
            settings: settings::Settings::new(&ctx.config),
            catalog: catalog::Catalog::new(),
            library: library_view::LibraryView::default(),
            downloads: downloads_view::DownloadsView::default(),
            title: None,
            reader: None,
            images,
            me: None,
            toast: None,
            dev_read: None,
            tab: Tab::Catalog,
            ctx,
        };
        let client = app.ctx.client.clone();
        let who = Task::perform(
            async move {
                if client.is_logged_in().await {
                    client.me().await.ok()
                } else {
                    None
                }
            },
            Message::Me,
        );
        let catalog = app.catalog.load(&app.ctx, false).map(Message::Catalog);
        let dev = app.dev_start();
        (app, Task::batch([who, catalog, dev]))
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    fn subscription(&self) -> Subscription<Message> {
        let downloads = Subscription::run(download_events).map(Message::DownloadEvent);
        let mut subs = vec![downloads];
        if self.reader.is_some() {
            subs.push(reader::keys().map(Message::Reader));
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tab(tab) => {
                self.tab = tab;
                self.title = None;
                self.close_reader();
                match tab {
                    Tab::Library => self.library.reload(&self.ctx, &mut self.images),
                    Tab::Downloads => {
                        self.downloads.reload(&self.ctx);
                        Task::none()
                    }
                    _ => Task::none(),
                }
            }
            Message::Catalog(msg) => {
                let (task, open) = self.catalog.update(msg, &self.ctx, &mut self.images);
                let task = task.map(Message::Catalog);
                match open {
                    Some((site, slug)) => Task::batch([task, self.open_title(site, slug)]),
                    None => task,
                }
            }
            Message::Title(msg) => {
                let loaded = matches!(msg, title::Msg::Loaded(Ok(_)));
                let Some(view) = self.title.as_mut() else {
                    return Task::none();
                };
                if loaded && let Some(n) = self.dev_read.take() {
                    let first = view.update(msg, &self.ctx, &mut self.images);
                    let task = match first {
                        title::Effect::Task(t) => t.map(Message::Title),
                        _ => Task::none(),
                    };
                    return Task::batch([task, self.update(Message::Title(title::Msg::Read(n)))]);
                }
                let effect = view.update(msg, &self.ctx, &mut self.images);
                match effect {
                    title::Effect::Task(task) => task.map(Message::Title),
                    title::Effect::Back => {
                        self.title = None;
                        if self.tab == Tab::Library {
                            return self.library.reload(&self.ctx, &mut self.images);
                        }
                        Task::none()
                    }
                    title::Effect::Read(start) => {
                        let (reader, task) = reader::Reader::open(start, &self.ctx, &mut self.images);
                        self.reader = Some(reader);
                        task.map(Message::Reader)
                    }
                    title::Effect::Toast(text) => {
                        self.toast = Some(text);
                        Task::none()
                    }
                }
            }
            Message::Reader(msg) => {
                let Some(reader) = self.reader.as_mut() else {
                    return Task::none();
                };
                match reader.update(msg, &self.ctx, &mut self.images) {
                    reader::Effect::Task(task) => task.map(Message::Reader),
                    reader::Effect::Close => {
                        self.close_reader();
                        if let Some(view) = self.title.as_mut() {
                            view.refresh_local(&self.ctx);
                        }
                        Task::none()
                    }
                }
            }
            Message::Library(msg) => match self.library.update(msg, &mut self.images) {
                (task, Some((site, slug))) => Task::batch([task, self.open_title(site, slug)]),
                (task, None) => task,
            },
            Message::Downloads(msg) => {
                if let Some(text) = self.downloads.update(msg, &self.ctx) {
                    self.toast = Some(text);
                }
                Task::none()
            }
            Message::Settings(msg) => {
                let (task, toast) = self.settings.update(msg, &mut self.ctx);
                if let Some(text) = toast {
                    self.toast = Some(text);
                }
                task
            }
            Message::DownloadEvent(event) => {
                self.downloads.on_event(&event, &self.ctx);
                if let Some(view) = self.title.as_mut() {
                    view.on_download_event(&event);
                }
                Task::none()
            }
            Message::Me(me) => {
                self.me = me;
                self.settings.me = self.me.clone();
                Task::none()
            }
            Message::DismissToast => {
                self.toast = None;
                Task::none()
            }
        }
    }

    /// Отладочный старт для проверки экранов без мыши:
    /// `POLKA_DEV_TAB=library|downloads|settings`,
    /// `POLKA_DEV_OPEN=mangalib:7965--chainsaw-man[:N]` открывает тайтл и, если задан
    /// номер, N-ю главу в порядке списка.
    fn dev_start(&mut self) -> Task<Message> {
        if let Ok(tab) = std::env::var("POLKA_DEV_TAB") {
            let tab = match tab.as_str() {
                "library" => Tab::Library,
                "downloads" => Tab::Downloads,
                "settings" => Tab::Settings,
                _ => Tab::Catalog,
            };
            return self.update(Message::Tab(tab));
        }
        let Ok(spec) = std::env::var("POLKA_DEV_OPEN") else {
            return Task::none();
        };
        let mut parts = spec.split(':');
        let (Some(site), Some(slug)) = (parts.next().and_then(Site::from_key), parts.next()) else {
            return Task::none();
        };
        self.dev_read = parts.next().and_then(|n| n.parse().ok());
        self.open_title(site, slug.to_string())
    }

    fn open_title(&mut self, site: Site, slug: String) -> Task<Message> {
        self.close_reader();
        let (view, task) = title::TitleView::open(site, slug, &self.ctx, &mut self.images);
        self.title = Some(view);
        task.map(Message::Title)
    }

    fn close_reader(&mut self) {
        if self.reader.take().is_some() {
            self.images.forget_pages();
        }
    }

    fn view(&self) -> Element<'_, Message> {
        if let Some(reader) = &self.reader {
            return reader.view(&self.images).map(Message::Reader);
        }
        let body: Element<'_, Message> = if let Some(view) = &self.title {
            view.view(&self.ctx, &self.images).map(Message::Title)
        } else {
            match self.tab {
                Tab::Catalog => self.catalog.view(&self.images).map(Message::Catalog),
                Tab::Library => self.library.view(&self.ctx, &self.images).map(Message::Library),
                Tab::Downloads => self.downloads.view().map(Message::Downloads),
                Tab::Settings => self.settings.view(&self.ctx).map(Message::Settings),
            }
        };
        let mut page = column![self.nav(), container(body).width(Fill).height(Fill)];
        if let Some(toast) = &self.toast {
            page = page.push(
                container(
                    row![
                        text(toast.as_str()).width(Fill),
                        button("Закрыть").on_press(Message::DismissToast).style(button::text)
                    ]
                    .spacing(12)
                    .align_y(iced::Alignment::Center),
                )
                .padding([8, 16])
                .width(Fill)
                .style(style::toast),
            );
        }
        page.into()
    }

    fn nav(&self) -> Element<'_, Message> {
        let tab = |label: &'static str, tab: Tab| {
            let active = self.tab == tab && self.title.is_none();
            button(text(label).size(15))
                .padding([8, 14])
                .style(if active { button::primary } else { button::text })
                .on_press(Message::Tab(tab))
        };
        let who = match &self.me {
            Some(me) => text(format!("@{}", me.username)).size(13).style(text::secondary),
            None => text("гость").size(13).style(text::secondary),
        };
        container(
            row![
                text("Полка").size(20),
                iced::widget::space().width(16),
                tab("Каталог", Tab::Catalog),
                tab("Библиотека", Tab::Library),
                tab("Загрузки", Tab::Downloads),
                tab("Настройки", Tab::Settings),
                iced::widget::space().width(Fill),
                who,
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        )
        .padding([6, 12])
        .style(style::bar)
        .width(Fill)
        .into()
    }
}

/// Поток событий загрузчика для подписки.
fn download_events() -> impl futures::Stream<Item = DownloadEvent> {
    let receiver = DOWNLOAD_EVENTS.get().map(|s| s.subscribe());
    futures::stream::unfold(receiver, |receiver| async move {
        let mut receiver = receiver?;
        loop {
            match receiver.recv().await {
                Ok(event) => return Some((event, Some(receiver))),
                // Отстали от потока: пропускаем потерянное, интерфейс перечитает базу.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// Сообщение об ошибке для человека, без цепочки служебных подробностей.
pub fn human_error(e: &impl std::fmt::Display) -> String {
    let s = e.to_string();
    if s.contains("error sending request") || s.contains("сеть") {
        "нет связи с сервером".into()
    } else {
        s
    }
}
