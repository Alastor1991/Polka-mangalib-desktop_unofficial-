//! Страница тайтла: рисует то, что посчитало ядро (`polka_core::title`), и передаёт
//! ему действия пользователя.

use std::path::PathBuf;

use iced::widget::{button, column, pick_list, row, scrollable, text};
use iced::{Alignment, Element, Fill, Task};
use libsocial::Site;
use libsocial::bookmarks::format_timecode;
use libsocial::models::{Episode, EpisodeDetails, PlayerKind};
use polka_core::db::{DownloadRow, DownloadState};
use polka_core::downloads::DownloadEvent;
use polka_core::library::{self, Job, episode_key};
use polka_core::reader::ReaderStart;
use polka_core::title::{self, Read, Resume, TitleData, TitlePage};
use polka_core::titles::title_row;

use super::catalog::cover;
use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, human_error};

/// Пункт выпадающего списка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: Option<u64>,
    pub label: String,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

pub struct TitleView {
    site: Site,
    slug: String,
    page: Option<TitlePage>,
    error: Option<String>,
    busy: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    /// Ответ приходит с адресом тайтла: устаревший ответ от предыдущего тайтла
    /// отбрасывается.
    Loaded(Site, String, Result<Box<TitleData>, String>),
    Retry,
    Back,
    ToggleLibrary,
    /// Ответ о закладке, вместе с адресом тайтла, как у `Loaded`.
    BookmarkLoaded(Site, String, Result<Option<Box<libsocial::Bookmark>>, String>),
    /// Закладку поменяли с этой страницы.
    BookmarkSaved(Site, String, Result<Option<Box<libsocial::Bookmark>>, String>),
    /// Выбрана папка закладок. `id: None` – убрать из закладок.
    Folder(Choice),
    /// Продолжить с места, записанного в закладке на сайте.
    Resume,
    OpenSite,
    Branch(Choice),
    Order,
    Read(usize),
    Continue,
    Download(usize),
    DownloadAll,
    Remove(usize),
    MarkRead(usize, bool),
    Export,
    Exported(Result<PathBuf, String>),
    Enqueued(Result<usize, String>, usize),
    Image(String, Loaded),
    Team(Choice),
    EpisodeLoaded(u64, EpisodeAction, Result<Box<EpisodeDetails>, String>),
    Watch(usize),
    DownloadEpisode(usize),
    Kodik(usize),
    Launched(Result<String, String>),
    /// Плеер закрыт, позиция просмотра записана в базу.
    PlaybackEnded,
}

pub enum Effect {
    Task(Task<Msg>),
    Back,
    Read(ReaderStart),
    Toast(String),
}

/// Что делать с эпизодом, когда подгрузятся его плееры.
#[derive(Debug, Clone, Copy)]
pub enum EpisodeAction {
    Teams,
    Watch,
    Download,
    Kodik,
    /// Отметить на сайте просмотренной: для этого нужен id плеера.
    MarkViewed,
}

fn none() -> Effect {
    Effect::Task(Task::none())
}

impl TitleView {
    pub fn open(site: Site, slug: String, ctx: &Ctx, images: &mut Images) -> (TitleView, Task<Msg>) {
        let view = TitleView {
            site,
            slug: slug.clone(),
            page: None,
            error: None,
            busy: None,
        };
        let cover = local_cover(ctx, site, &slug).map(|p| images.request(ImageReq::File(p), Msg::Image));
        let load = view.load(ctx);
        (view, Task::batch(cover.into_iter().chain([load])))
    }

    fn load(&self, ctx: &Ctx) -> Task<Msg> {
        let (site, slug, core) = (self.site, self.slug.clone(), ctx.clone());
        Task::perform(
            {
                let slug = slug.clone();
                async move {
                    title::load(&core, site, &slug)
                        .await
                        .map(Box::new)
                        .map_err(|e| human_error(&e))
                }
            },
            move |r| Msg::Loaded(site, slug.clone(), r),
        )
    }

    /// Запуск плеера закончился: убрать надпись «Запускаю плеер…».
    pub fn launched(&mut self) {
        self.busy = None;
    }

    /// Перечитывает из базы загрузки, прогресс и библиотеку.
    pub fn refresh_local(&mut self, ctx: &Ctx) {
        if let Some(page) = &mut self.page {
            page.refresh_local(ctx);
        }
    }

    pub fn on_download_event(&mut self, e: &DownloadEvent) {
        if let Some(page) = &mut self.page {
            page.on_download_event(e);
        }
    }

    /// Пришли данные аккаунта: если страницу открыли раньше, догружаем закладку.
    pub fn on_account(&self, ctx: &Ctx) -> Task<Msg> {
        match &self.page {
            Some(page) if !page.data.offline && !page.bookmark_known => self.load_bookmark(ctx),
            _ => Task::none(),
        }
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> Effect {
        match msg {
            Msg::Loaded(site, slug, _) | Msg::BookmarkLoaded(site, slug, _) | Msg::BookmarkSaved(site, slug, _)
                if site != self.site || slug != self.slug =>
            {
                none()
            }
            Msg::Loaded(_, _, Ok(data)) => {
                self.error = None;
                let page = TitlePage::new(ctx, self.site, self.slug.clone(), *data);
                let mut tasks = Vec::new();
                if let Some(url) = page.data.details.short.cover.best() {
                    tasks.push(images.request(
                        ImageReq::Url {
                            site: self.site,
                            url: url.to_string(),
                        },
                        Msg::Image,
                    ));
                }
                let online = !page.data.offline;
                // Для аниме команды берём из первой серии: у какой есть свой плеер.
                let first = page.data.episodes.first().map(|e| e.id);
                self.page = Some(page);
                if online {
                    tasks.push(self.load_bookmark(ctx));
                    if self.site.is_anime()
                        && let Some(id) = first
                    {
                        tasks.push(self.load_episode(ctx, id, EpisodeAction::Teams));
                    }
                }
                Effect::Task(Task::batch(tasks))
            }
            Msg::Loaded(_, _, Err(e)) => {
                self.error = Some(e);
                none()
            }
            Msg::Retry => {
                self.error = None;
                Effect::Task(self.load(ctx))
            }
            Msg::Back => Effect::Back,
            Msg::Image(key, loaded) => Effect::Task(images.finish(key, loaded, Msg::Image)),
            Msg::Launched(result) => {
                self.busy = None;
                let (Ok(text) | Err(text)) = result;
                Effect::Toast(text)
            }
            msg => {
                // Остальное имеет смысл только на загруженной странице.
                let Some(page) = self.page.take() else {
                    return none();
                };
                let mut page = page;
                let effect = self.on_page(&mut page, msg, ctx);
                self.page = Some(page);
                effect
            }
        }
    }

    fn on_page(&mut self, page: &mut TitlePage, msg: Msg, ctx: &Ctx) -> Effect {
        match msg {
            Msg::ToggleLibrary => match page.toggle_local_library(ctx) {
                Ok(_) => none(),
                Err(e) => Effect::Toast(format!("Не сохранилось: {e:#}")),
            },
            Msg::BookmarkLoaded(_, _, Ok(bookmark)) => {
                page.bookmark_loaded(ctx, bookmark.map(|b| *b));
                none()
            }
            Msg::BookmarkLoaded(_, _, Err(e)) => Effect::Toast(format!("Закладка не загрузилась: {e}")),
            Msg::BookmarkSaved(_, _, Ok(bookmark)) => {
                self.busy = None;
                page.bookmark_saved(ctx, bookmark.map(|b| *b));
                none()
            }
            Msg::BookmarkSaved(_, _, Err(e)) => {
                self.busy = None;
                Effect::Toast(format!("Закладка не сохранилась: {e}"))
            }
            Msg::Folder(choice) => self.set_folder(page, ctx, choice.id),
            Msg::Resume => {
                let Some(resume) = page.resume() else { return none() };
                let Some(index) = page.prepare_resume(ctx, &resume) else {
                    return none();
                };
                match resume {
                    Resume::Chapter { .. } => self.read(page, ctx, index),
                    Resume::Episode { .. } => self.episode_action(page, ctx, index, EpisodeAction::Watch),
                }
            }
            Msg::OpenSite => {
                let _ = open::that_detached(self.site.title_url(&self.slug));
                none()
            }
            Msg::Branch(choice) => {
                page.branch = choice.id;
                none()
            }
            Msg::Order => {
                page.newest_first = !page.newest_first;
                none()
            }
            Msg::Read(index) => self.read(page, ctx, index),
            Msg::Continue => match page.continue_target().and_then(|id| page.index_of_chapter(id)) {
                Some(index) => self.read(page, ctx, index),
                None => none(),
            },
            Msg::Download(index) => {
                let Some(chapter) = page.chapter_at(index) else {
                    return none();
                };
                let (jobs, locked) = page.chapter_jobs(&[chapter]);
                self.enqueue(page, ctx, jobs, locked)
            }
            Msg::DownloadAll => {
                let chapters = page.chapters();
                let (jobs, locked) = page.chapter_jobs(&chapters);
                self.enqueue(page, ctx, jobs, locked)
            }
            Msg::Remove(index) => {
                let key = if self.site.is_anime() {
                    page.episode_at(index).map(|e| episode_key(e.id))
                } else {
                    page.chapter_at(index).map(|c| page.chapter_key_of(c))
                };
                if let Some(key) = key {
                    if let Err(e) = ctx.downloader.remove(self.site, &self.slug, &key) {
                        return Effect::Toast(format!("Не удалилось: {e:#}"));
                    }
                    page.downloads.remove(&key);
                }
                none()
            }
            Msg::MarkRead(index, finished) => match page.mark_read(ctx, index, finished) {
                // Плееры серии ещё не загружены: без них сайт отметку не примет.
                Some(id) => Effect::Task(self.load_episode(ctx, id, EpisodeAction::MarkViewed)),
                None => none(),
            },
            Msg::Export => self.export(page, ctx),
            Msg::Exported(result) => {
                self.busy = None;
                match result {
                    Ok(dir) => {
                        let _ = open::that_detached(&dir);
                        Effect::Toast(format!("CBZ сохранены в {}", dir.display()))
                    }
                    Err(e) => Effect::Toast(format!("Экспорт не удался: {e}")),
                }
            }
            Msg::Enqueued(result, locked) => {
                page.refresh_local(ctx);
                match result {
                    Ok(added) if added + locked > 1 || locked > 0 => {
                        let mut text = format!("В очередь добавлено глав: {added}");
                        if locked > 0 {
                            text.push_str(&format!(", закрытых пропущено: {locked}"));
                        }
                        Effect::Toast(text)
                    }
                    Ok(_) => none(),
                    Err(e) => Effect::Toast(format!("Не поставилось в очередь: {e}")),
                }
            }
            Msg::Team(choice) => {
                page.team = choice.id;
                none()
            }
            Msg::EpisodeLoaded(id, action, result) => self.episode_loaded(page, ctx, id, action, result),
            Msg::Watch(i) => self.episode_action(page, ctx, i, EpisodeAction::Watch),
            Msg::DownloadEpisode(i) => self.episode_action(page, ctx, i, EpisodeAction::Download),
            Msg::Kodik(i) => self.episode_action(page, ctx, i, EpisodeAction::Kodik),
            Msg::PlaybackEnded => {
                page.refresh_local(ctx);
                none()
            }
            // Разобраны в `update`.
            Msg::Loaded(..) | Msg::Retry | Msg::Back | Msg::Image(..) | Msg::Launched(_) => none(),
        }
    }

    fn read(&self, page: &TitlePage, ctx: &Ctx, index: usize) -> Effect {
        match page.reader_start(ctx, index) {
            Some(start) => Effect::Read(start),
            None => none(),
        }
    }

    /// Ставит задания в очередь одной транзакцией в фоне: на сотнях глав запись в
    /// базу иначе подвесила бы окно.
    fn enqueue(&self, page: &TitlePage, ctx: &Ctx, jobs: Vec<(String, Job)>, locked: usize) -> Effect {
        let title = title_row(self.site, &page.data.details);
        let downloader = ctx.downloader.clone();
        Effect::Task(Task::perform(
            async move {
                tokio::task::spawn_blocking(move || downloader.enqueue_many(&title, jobs))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("{e:#}"))
            },
            move |r| Msg::Enqueued(r, locked),
        ))
    }

    fn export(&mut self, page: &TitlePage, ctx: &Ctx) -> Effect {
        if self.busy.is_some() {
            return none();
        }
        let plan = page.export_plan();
        if plan.is_empty() {
            return Effect::Toast("Сначала скачайте главы: экспорт собирает CBZ из скачанного.".into());
        }
        let name = page.data.details.short.display_name().to_string();
        let out_dir = ctx.library.root().join("export").join(library::safe(&name));
        self.busy = Some("Собираю CBZ…".into());
        Effect::Task(Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    for (volume, chapters) in plan {
                        // Расширение дописывается после обрезки длинного имени.
                        let file = out_dir.join(format!("{}.cbz", library::safe(&format!("{name} – Том {volume}"))));
                        library::export_cbz(&name, &chapters, &file)?;
                    }
                    Ok::<PathBuf, anyhow::Error>(out_dir)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{e:#}"))
            },
            Msg::Exported,
        ))
    }

    fn load_bookmark(&self, ctx: &Ctx) -> Task<Msg> {
        if ctx.user_id.is_none() {
            return Task::none();
        }
        let (site, slug, client) = (self.site, self.slug.clone(), ctx.client.clone());
        Task::perform(
            {
                let slug = slug.clone();
                async move {
                    client
                        .bookmark(site, &slug)
                        .await
                        .map(|b| b.map(Box::new))
                        .map_err(|e| human_error(&e))
                }
            },
            move |r| Msg::BookmarkLoaded(site, slug.clone(), r),
        )
    }

    fn set_folder(&mut self, page: &TitlePage, ctx: &Ctx, folder: Option<u64>) -> Effect {
        if self.busy.is_some() {
            return none();
        }
        self.busy = Some("Сохраняю закладку…".into());
        let (site, slug, core) = (self.site, self.slug.clone(), ctx.clone());
        let current = page.bookmark.as_ref().map(|b| b.id);
        Effect::Task(Task::perform(
            {
                let slug = slug.clone();
                async move {
                    title::set_folder(&core, site, &slug, folder, current)
                        .await
                        .map(|b| b.map(Box::new))
                        .map_err(|e| human_error(&e))
                }
            },
            move |r| Msg::BookmarkSaved(site, slug.clone(), r),
        ))
    }

    // --- Серии ---

    fn load_episode(&self, ctx: &Ctx, id: u64, action: EpisodeAction) -> Task<Msg> {
        let client = ctx.client.clone();
        Task::perform(
            async move { client.episode(id).await.map(Box::new).map_err(|e| human_error(&e)) },
            move |r| Msg::EpisodeLoaded(id, action, r),
        )
    }

    fn episode_action(&mut self, page: &mut TitlePage, ctx: &Ctx, index: usize, action: EpisodeAction) -> Effect {
        let Some(episode) = page.episode_at(index).cloned() else {
            return none();
        };
        // Скачанную серию смотрим с диска, сеть не нужна.
        if matches!(action, EpisodeAction::Watch)
            && let Some((path, player_id)) = page.downloaded_player(episode.id)
        {
            let player = page
                .episodes
                .get(&episode.id)
                .and_then(|d| d.players.iter().find(|p| p.id == player_id))
                .cloned();
            let start = page.resume_at(episode.id);
            page.mark_started(ctx, episode.id);
            let mut track = page.track(ctx, &episode, player.as_ref());
            track.player_id = track.player_id.or(Some(player_id));
            return Effect::Task(watch_player(
                polka_core::player::play_file(path, start, episode.title()),
                track,
            ));
        }
        match page.episodes.get(&episode.id).cloned() {
            Some(details) => self.run_episode_action(page, ctx, &episode, &details, action),
            None => {
                self.busy = Some("Загружаю серию…".into());
                Effect::Task(self.load_episode(ctx, episode.id, action))
            }
        }
    }

    fn episode_loaded(
        &mut self,
        page: &mut TitlePage,
        ctx: &Ctx,
        id: u64,
        action: EpisodeAction,
        result: Result<Box<EpisodeDetails>, String>,
    ) -> Effect {
        // Ответ мог прийти от серии другого тайтла, если страницу успели сменить.
        let Some(episode) = page.data.episodes.iter().find(|e| e.id == id).cloned() else {
            return none();
        };
        self.busy = None;
        let details = match result {
            Ok(d) => *d,
            Err(e) => return Effect::Toast(format!("Серия не загрузилась: {e}")),
        };
        page.add_episode(details.clone());
        self.run_episode_action(page, ctx, &episode, &details, action)
    }

    fn run_episode_action(
        &mut self,
        page: &mut TitlePage,
        ctx: &Ctx,
        episode: &Episode,
        details: &EpisodeDetails,
        action: EpisodeAction,
    ) -> Effect {
        match action {
            EpisodeAction::Teams => none(),
            EpisodeAction::MarkViewed => {
                if let Some(index) = page.episodes_shown().iter().position(|e| e.id == episode.id) {
                    page.push_view_mark(ctx, index);
                }
                none()
            }
            EpisodeAction::Kodik => {
                // Открываем в браузере только https: адрес пришёл с сервера.
                match page
                    .kodik_player(details)
                    .and_then(|p| p.embed_url())
                    .filter(|u| u.starts_with("https://"))
                {
                    Some(url) => {
                        let _ = open::that_detached(url);
                        none()
                    }
                    None => Effect::Toast("У этой серии нет плеера Kodik".into()),
                }
            }
            EpisodeAction::Watch => {
                let Some(player) = page.own_player(details).cloned() else {
                    let has_kodik = details.players.iter().any(|p| p.kind() == PlayerKind::Kodik);
                    return Effect::Toast(if has_kodik {
                        "Своего плеера AnimeLib у серии нет. Можно открыть Kodik в браузере.".into()
                    } else {
                        "Видео AnimeLib доступно после входа (Настройки → Войти).".into()
                    });
                };
                let start = page.resume_at(episode.id);
                page.mark_started(ctx, episode.id);
                let track = page.track(ctx, episode, Some(&player));
                self.busy = Some("Запускаю плеер…".into());
                let client = ctx.client.clone();
                let quality = ctx.config.max_quality;
                let title = format!("{} – {}", page.data.details.short.display_name(), episode.title());
                Effect::Task(watch_player(
                    async move {
                        let video = client
                            .resolve_video(&player, quality, None, true)
                            .await
                            .map_err(|e| human_error(&e))?;
                        polka_core::player::play_stream(video, start, title).await
                    },
                    track,
                ))
            }
            EpisodeAction::Download => {
                let Some(player) = page.own_player(details) else {
                    return Effect::Toast("Скачать можно только серии с плеером AnimeLib".into());
                };
                let title = title_row(self.site, &page.data.details);
                let label = format!("{} ({})", episode.title(), player.team_name());
                match ctx
                    .downloader
                    .enqueue(&title, &label, Job::episode(episode, player, ctx.config.max_quality))
                {
                    Ok(_) => {
                        page.refresh_local(ctx);
                        none()
                    }
                    Err(e) => Effect::Toast(format!("Не поставилось в очередь: {e:#}")),
                }
            }
        }
    }

    // --- Вид ---

    pub fn view<'a>(&'a self, ctx: &'a Ctx, images: &'a Images) -> Element<'a, Msg> {
        let back = button("← Назад").style(button::text).on_press(Msg::Back);
        let Some(page) = &self.page else {
            let status: Element<'a, Msg> = match &self.error {
                Some(e) => row![
                    text(format!("Не загрузилось: {e}")),
                    button("Ещё раз").on_press(Msg::Retry)
                ]
                .spacing(12)
                .align_y(Alignment::Center)
                .into(),
                None => text("Загружаю…").style(text::secondary).into(),
            };
            return column![back, status].spacing(12).padding(16).into();
        };
        let data = &page.data;
        let d = &data.details;
        let cover_req = local_cover(ctx, self.site, &self.slug)
            .filter(|p| images.get(&ImageReq::File(p.clone()).key()).is_some())
            .map(ImageReq::File)
            .or_else(|| {
                d.short.cover.best().map(|u| ImageReq::Url {
                    site: self.site,
                    url: u.to_string(),
                })
            });

        let mut facts = Vec::new();
        if let Some(k) = &d.short.kind {
            facts.push(k.label.clone());
        }
        if let Some(s) = &d.short.status {
            facts.push(s.label.clone());
        }
        if let Some(y) = &d.short.release_date {
            facts.push(y.clone());
        }
        if let Some(a) = &d.short.age_restriction {
            facts.push(a.label.clone());
        }
        if let Some(r) = d.short.rating.as_ref().filter(|r| r.average > 0.0) {
            facts.push(format!("★ {:.1} ({} голосов)", r.average, r.votes));
        }
        let genres: Vec<&str> = d.genres.iter().map(|g| g.name.as_str()).collect();

        let folders = ctx.folders_for(self.site);
        let library: Element<'a, Msg> = if page.bookmark_known && !folders.is_empty() && !data.offline {
            // С входом библиотека – это закладки сайта с его папками.
            let mut choices: Vec<Choice> = folders
                .iter()
                .map(|f| Choice {
                    id: Some(f.id),
                    label: f.name.clone(),
                })
                .collect();
            let selected = page
                .bookmark
                .as_ref()
                .and_then(|b| choices.iter().find(|c| c.id == Some(b.status)))
                .cloned();
            if page.bookmark.is_some() {
                choices.push(Choice {
                    id: None,
                    label: "Убрать из закладок".into(),
                });
            }
            pick_list(choices, selected, Msg::Folder)
                .placeholder("+ В закладки")
                .into()
        } else {
            button(if page.in_library {
                "✓ В библиотеке"
            } else {
                "+ В библиотеку"
            })
            .style(if page.in_library {
                button::secondary
            } else {
                button::primary
            })
            .on_press(Msg::ToggleLibrary)
            .into()
        };
        let mut actions = row![
            back,
            library,
            button("Открыть на сайте")
                .style(button::secondary)
                .on_press(Msg::OpenSite),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let resume = page.resume().and_then(|r| resume_label(page, &r));
        if let Some(label) = &resume {
            actions = actions.push(button(text(label.clone())).on_press(Msg::Resume));
        }
        if !self.site.is_anime() {
            if resume.is_none() {
                actions = actions.push(button("Читать").on_press(Msg::Continue));
            }
            actions = actions
                .push(
                    button("Скачать все")
                        .style(button::secondary)
                        .on_press(Msg::DownloadAll),
                )
                .push(button("Экспорт CBZ").style(button::secondary).on_press(Msg::Export));
        }

        let mut info = column![
            text(d.short.display_name()).size(26),
            text(d.short.eng_name.as_deref().unwrap_or(&d.short.name))
                .size(14)
                .style(text::secondary),
            text(facts.join(" • ")).size(14),
            text(genres.join(", ")).size(13).style(text::secondary),
            actions,
        ]
        .spacing(8)
        .width(Fill);
        if data.offline {
            info = info.push(text("Нет связи: показано скачанное").size(13).style(text::danger));
        }
        if let Some(busy) = &self.busy {
            info = info.push(text(busy.as_str()).size(13).style(text::secondary));
        }

        let header = row![cover(images, cover_req.as_ref(), 190.0, 270.0), info].spacing(20);
        let summary = text(d.summary_text()).size(14);
        let list = if self.site.is_anime() {
            episodes_view(page)
        } else {
            chapters_view(page)
        };
        scrollable(column![header, summary, list].spacing(18).padding(20).max_width(1100))
            .height(Fill)
            .width(Fill)
            .into()
    }
}

fn order_button<'a>(page: &TitlePage) -> iced::widget::Button<'a, Msg> {
    button(if page.newest_first {
        "Сначала новые"
    } else {
        "Сначала первые"
    })
    .style(button::text)
    .on_press(Msg::Order)
}

fn chapters_view(page: &TitlePage) -> Element<'_, Msg> {
    let mut controls = row![
        text(format!("Глав: {}", page.chapters_asc().len())).size(16),
        order_button(page),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let branches = page.branch_choices();
    if !branches.is_empty() {
        let choices: Vec<Choice> = branches
            .into_iter()
            .map(|(id, label)| Choice { id: Some(id), label })
            .collect();
        let selected = choices.iter().find(|c| c.id == page.branch).cloned();
        controls = controls
            .push(text("Перевод:").size(14))
            .push(pick_list(choices, selected, Msg::Branch));
    }
    let rows = page.chapter_rows().into_iter().enumerate().map(|(i, r)| {
        let mut status = Vec::new();
        if !r.open {
            status.push(match r.opens_at.map(short_date) {
                Some(d) => format!("🔒 откроется {d}"),
                None => "🔒 платная".into(),
            });
        }
        match r.read {
            Read::Finished => status.push("✓ прочитано".into()),
            Read::Started { position, total } if total > 0 => status.push(format!("стр. {}/{}", position + 1, total)),
            _ => {}
        }
        if let Some(row) = r.download {
            status.push(download_label(row));
        }
        let read_done = r.read == Read::Finished;
        row![
            column![
                text(r.chapter.title()).size(15),
                text(r.info.and_then(|b| b.translator()).unwrap_or("").to_string())
                    .size(12)
                    .style(text::secondary),
            ]
            .width(Fill),
            text(status.join("  ")).size(13).style(text::secondary),
            button(if read_done { "↺" } else { "✓" })
                .style(button::text)
                .on_press(Msg::MarkRead(i, !read_done)),
            button("Читать").on_press_maybe(r.open.then_some(Msg::Read(i))),
            if r.download.is_some() {
                button("Удалить").style(button::danger).on_press(Msg::Remove(i))
            } else {
                button("Скачать")
                    .style(button::secondary)
                    .on_press_maybe(r.open.then_some(Msg::Download(i)))
            },
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding([6, 10])
        .into()
    });
    column![controls, column(rows).spacing(2)].spacing(10).into()
}

fn episodes_view(page: &TitlePage) -> Element<'_, Msg> {
    let mut controls = row![
        text(format!("Серий: {}", page.data.episodes.len())).size(16),
        order_button(page),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if !page.teams.is_empty() {
        let choices: Vec<Choice> = page
            .teams
            .iter()
            .map(|t| Choice {
                id: Some(t.id),
                label: format!("{} ({})", t.name, t.translation),
            })
            .collect();
        let selected = choices.iter().find(|c| c.id == page.team).cloned();
        controls = controls
            .push(text("Озвучка:").size(14))
            .push(pick_list(choices, selected, Msg::Team));
    }
    let rows = page.episode_rows().into_iter().enumerate().map(|(i, r)| {
        let mut status = Vec::new();
        match r.read {
            Read::Finished => status.push("✓ просмотрено".to_string()),
            Read::Started { position, .. } if position > 0 => {
                status.push(format!("остановились на {}", clock(position)))
            }
            Read::Started { .. } => status.push("начато".to_string()),
            Read::None => {}
        }
        if let Some(row) = r.download {
            status.push(download_label(row));
        }
        let watched = r.read == Read::Finished;
        row![
            text(r.episode.title()).size(15).width(Fill),
            text(status.join("  ")).size(13).style(text::secondary),
            button(if watched { "↺" } else { "✓" })
                .style(button::text)
                .on_press(Msg::MarkRead(i, !watched)),
            button("Смотреть").on_press(Msg::Watch(i)),
            button("Kodik").style(button::text).on_press(Msg::Kodik(i)),
            if r.download.is_some() {
                button("Удалить").style(button::danger).on_press(Msg::Remove(i))
            } else {
                button("Скачать")
                    .style(button::secondary)
                    .on_press(Msg::DownloadEpisode(i))
            },
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding([6, 10])
        .into()
    });
    column![controls, column(rows).spacing(2)].spacing(10).into()
}

/// Подпись кнопки «Продолжить».
fn resume_label(page: &TitlePage, resume: &Resume) -> Option<String> {
    match *resume {
        Resume::Chapter {
            chapter_id, page: p, ..
        } => {
            let chapter = page.data.chapters.iter().find(|c| c.id == chapter_id)?;
            Some(match p {
                Some(p) => format!("Продолжить: том {} глава {}, стр. {p}", chapter.volume, chapter.number),
                None => format!("Продолжить: том {} глава {}", chapter.volume, chapter.number),
            })
        }
        Resume::Episode { episode_id, secs, .. } => {
            let episode = page.data.episodes.iter().find(|e| e.id == episode_id)?;
            Some(match secs {
                Some(secs) => format!("Продолжить: {}, {}", episode.title(), format_timecode(secs)),
                None => format!("Продолжить: {}", episode.title()),
            })
        }
    }
}

fn download_label(row: &DownloadRow) -> String {
    match row.state {
        DownloadState::Done => "⬇ скачано".into(),
        DownloadState::Queued => "в очереди".into(),
        DownloadState::Paused => "пауза".into(),
        DownloadState::Failed => "ошибка загрузки".into(),
        DownloadState::Running if row.total > 10_000 => {
            format!("⬇ {}%", row.done * 100 / row.total)
        }
        DownloadState::Running if row.total > 0 => format!("⬇ {}/{}", row.done, row.total),
        DownloadState::Running => "⬇ …".into(),
    }
}

/// Секунды как `12:34` или `1:02:03`.
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Запускает плеер и следит за ним в фоне: сначала сообщает о запуске, а после
/// закрытия плеера просит перечитать прогресс.
fn watch_player(
    launch: impl Future<Output = Result<polka_core::player::Playback, String>> + Send + 'static,
    track: polka_core::player::Track,
) -> Task<Msg> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(async move {
        match launch.await {
            Ok(playback) => {
                let _ = tx.unbounded_send(Msg::Launched(Ok(playback.message.clone())));
                playback.track(track).await;
                let _ = tx.unbounded_send(Msg::PlaybackEnded);
            }
            Err(e) => {
                let _ = tx.unbounded_send(Msg::Launched(Err(e)));
            }
        }
    });
    Task::run(rx, |msg| msg)
}

/// `2026-10-13T17:05:00.000000Z` → `13.10.2026`.
fn short_date(iso: &str) -> String {
    let date = iso.get(..10).unwrap_or(iso);
    let parts: Vec<&str> = date.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => format!("{d}.{m}.{y}"),
        _ => date.to_string(),
    }
}

fn local_cover(ctx: &Ctx, site: Site, slug: &str) -> Option<PathBuf> {
    let path = ctx.library.cover_path(site, slug);
    path.is_file().then_some(path)
}
