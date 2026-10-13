//! Страница тайтла: описание, главы или серии, чтение, скачивание, видео.

use std::collections::HashMap;
use std::path::PathBuf;

use iced::widget::{button, column, pick_list, row, scrollable, text};
use iced::{Alignment, Element, Fill, Task};
use libsocial::bookmarks::{format_timecode, parse_timecode};
use libsocial::models::{Branch, Chapter, Episode, EpisodeDetails, Player, PlayerKind, TitleDetails};
use libsocial::{Bookmark, Site, mirrors};

use super::catalog::cover;
use super::images::{ImageReq, Images, Loaded};
use super::reader::ReaderStart;
use super::{Ctx, human_error};
use polka_core::db::{DownloadRow, DownloadState, Progress};
use polka_core::downloads::DownloadEvent;
use polka_core::library::{self, ExportChapter, Job, chapter_key, episode_key};
use polka_core::sync::Mark;
use polka_core::titles::{pick_branch, title_row};

#[derive(Debug, Clone)]
pub struct TitleData {
    pub details: TitleDetails,
    pub chapters: Vec<Chapter>,
    pub branches: Vec<Branch>,
    pub episodes: Vec<Episode>,
    /// Данные собраны из библиотеки на диске, сеть недоступна.
    pub offline: bool,
}

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
    data: Option<TitleData>,
    error: Option<String>,
    branch: Option<u64>,
    downloads: HashMap<String, DownloadRow>,
    progress: HashMap<String, Progress>,
    in_library: bool,
    newest_first: bool,
    /// Аниме: команды с собственным плеером и выбранная из них.
    teams: Vec<Choice>,
    team: Option<u64>,
    episodes: HashMap<u64, EpisodeDetails>,
    busy: Option<String>,
    /// Закладка на сайте. `None` – тайтла нет в закладках или закладка ещё не пришла.
    bookmark: Option<Bookmark>,
    /// Ответ о закладке получен: до этого выбор папки не показываем.
    bookmark_known: bool,
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
    BookmarkLoaded(Site, String, Result<Option<Box<Bookmark>>, String>),
    /// Закладку поменяли с этой страницы.
    BookmarkSaved(Site, String, Result<Option<Box<Bookmark>>, String>),
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
    EpisodeLoaded(u64, EpisodeAction, Result<EpisodeDetails, String>),
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

/// Куда ведёт «Продолжить» у манги.
struct MangaResume {
    /// Id главы в списке (`Chapter::id`).
    chapter_id: u64,
    branch: Option<u64>,
    /// Страница с единицы. `None` – с начала главы.
    page: Option<u64>,
    /// Страница взята из закладки: своего места на этом устройстве нет.
    from_bookmark: bool,
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

impl TitleView {
    pub fn open(site: Site, slug: String, ctx: &Ctx, images: &mut Images) -> (TitleView, Task<Msg>) {
        let mut view = TitleView {
            site,
            slug: slug.clone(),
            data: None,
            error: None,
            branch: None,
            downloads: HashMap::new(),
            progress: HashMap::new(),
            in_library: false,
            newest_first: true,
            teams: Vec::new(),
            team: None,
            episodes: HashMap::new(),
            busy: None,
            bookmark: None,
            bookmark_known: false,
        };
        view.refresh_local(ctx);
        let cover = local_cover(ctx, site, &slug).map(|p| images.request(ImageReq::File(p), Msg::Image));
        let load = view.load(ctx);
        (view, Task::batch(cover.into_iter().chain([load])))
    }

    fn load(&self, ctx: &Ctx) -> Task<Msg> {
        let (site, slug) = (self.site, self.slug.clone());
        Task::perform(load_title(ctx.clone(), site, slug.clone()), move |r| {
            Msg::Loaded(site, slug.clone(), r.map(Box::new))
        })
    }

    /// Запуск плеера закончился: убрать надпись «Запускаю плеер…».
    pub fn launched(&mut self) {
        self.busy = None;
    }

    /// Перечитывает с диска состояние загрузок, прогресса и библиотеки.
    pub fn refresh_local(&mut self, ctx: &Ctx) {
        if let Ok(rows) = ctx.db.downloads_for(self.site, &self.slug) {
            self.downloads = rows.into_iter().map(|r| (r.item.clone(), r)).collect();
        }
        if let Ok(progress) = ctx.db.progress_for(self.site, &self.slug) {
            self.progress = progress.into_iter().collect();
        }
        self.in_library = ctx
            .db
            .title(self.site, &self.slug)
            .ok()
            .flatten()
            .is_some_and(|t| t.in_library);
    }

    pub fn on_download_event(&mut self, e: &DownloadEvent) {
        if e.site != self.site || e.slug != self.slug {
            return;
        }
        if e.removed {
            self.downloads.remove(&e.item);
            return;
        }
        if let Some(row) = self.downloads.get_mut(&e.item) {
            row.state = e.state;
            if e.total > 0 {
                row.done = e.done;
                row.total = e.total;
            }
            row.error = e.error.clone();
        }
    }

    pub fn update(&mut self, msg: Msg, ctx: &Ctx, images: &mut Images) -> Effect {
        match msg {
            Msg::Loaded(site, slug, _) if site != self.site || slug != self.slug => Effect::Task(Task::none()),
            Msg::Loaded(_, _, Ok(data)) => {
                let data = *data;
                self.error = None;
                let mut tasks = Vec::new();
                if let Some(url) = data.details.short.cover.best() {
                    tasks.push(images.request(
                        ImageReq::Url {
                            site: self.site,
                            url: url.to_string(),
                        },
                        Msg::Image,
                    ));
                }
                if self.branch.is_none() {
                    self.branch = data
                        .branches
                        .first()
                        .map(|b| b.id)
                        .filter(|_| has_multi(&data.chapters));
                }
                if !data.offline {
                    let _ = ctx.db.upsert_title(&title_row(self.site, &data.details));
                    tasks.push(self.load_bookmark(ctx));
                }
                // Для аниме команды берём из первой серии: у какой есть свой плеер.
                if self.site.is_anime()
                    && let Some(first) = data.episodes.first()
                    && !data.offline
                {
                    tasks.push(self.load_episode(ctx, first.id, EpisodeAction::Teams));
                }
                self.data = Some(data);
                Effect::Task(Task::batch(tasks))
            }
            Msg::Loaded(_, _, Err(e)) => {
                self.error = Some(e);
                Effect::Task(Task::none())
            }
            Msg::Retry => {
                self.error = None;
                Effect::Task(self.load(ctx))
            }
            Msg::Back => Effect::Back,
            Msg::ToggleLibrary => {
                let Some(data) = &self.data else {
                    return Effect::Task(Task::none());
                };
                let _ = ctx.db.upsert_title(&title_row(self.site, &data.details));
                let value = !self.in_library;
                match ctx.db.set_in_library(self.site, &self.slug, value) {
                    Ok(()) => self.in_library = value,
                    Err(e) => return Effect::Toast(format!("Не сохранилось: {e:#}")),
                }
                Effect::Task(Task::none())
            }
            Msg::BookmarkLoaded(site, slug, _) | Msg::BookmarkSaved(site, slug, _)
                if site != self.site || slug != self.slug =>
            {
                Effect::Task(Task::none())
            }
            Msg::BookmarkLoaded(_, _, Ok(bookmark)) => {
                // Тайтл в закладках – значит, и в библиотеке на устройстве, чтобы
                // открываться без сети. Флаг, поставленный без входа, не снимаем.
                if bookmark.is_some() {
                    self.set_local_library(ctx, true);
                }
                self.bookmark = bookmark.map(|b| *b);
                self.bookmark_known = true;
                Effect::Task(Task::none())
            }
            Msg::BookmarkSaved(_, _, Ok(bookmark)) => {
                self.busy = None;
                self.set_local_library(ctx, bookmark.is_some());
                self.bookmark = bookmark.map(|b| *b);
                Effect::Task(Task::none())
            }
            Msg::BookmarkLoaded(_, _, Err(e)) => Effect::Toast(format!("Закладка не загрузилась: {e}")),
            Msg::BookmarkSaved(_, _, Err(e)) => {
                self.busy = None;
                Effect::Toast(format!("Закладка не сохранилась: {e}"))
            }
            Msg::Folder(choice) => self.set_folder(ctx, choice.id),
            Msg::Resume => self.resume(ctx),
            Msg::OpenSite => {
                let _ = open::that_detached(self.site.title_url(&self.slug));
                Effect::Task(Task::none())
            }
            Msg::Branch(choice) => {
                self.branch = choice.id;
                Effect::Task(Task::none())
            }
            Msg::Order => {
                self.newest_first = !self.newest_first;
                Effect::Task(Task::none())
            }
            Msg::Read(index) => self.read(ctx, index),
            Msg::Continue => {
                let Some(id) = self.continue_target() else {
                    return Effect::Task(Task::none());
                };
                match self.visible_chapters().iter().position(|c| c.id == id) {
                    Some(index) => self.read(ctx, index),
                    None => Effect::Task(Task::none()),
                }
            }
            Msg::Download(index) => {
                let chapters = self.visible_chapters();
                let Some(chapter) = chapters.get(index) else {
                    return Effect::Task(Task::none());
                };
                let chapter = (*chapter).clone();
                self.enqueue_chapters(ctx, &[chapter])
            }
            Msg::DownloadAll => {
                let chapters: Vec<Chapter> = self.visible_chapters().into_iter().cloned().collect();
                self.enqueue_chapters(ctx, &chapters)
            }
            Msg::Remove(index) => {
                let key = if self.site.is_anime() {
                    self.episode_at(index).map(|e| episode_key(e.id))
                } else {
                    self.visible_chapters()
                        .get(index)
                        .map(|c| chapter_key(c.id, self.branch_of(c)))
                };
                if let Some(key) = key {
                    if let Err(e) = ctx.downloader.remove(self.site, &self.slug, &key) {
                        return Effect::Toast(format!("Не удалилось: {e:#}"));
                    }
                    self.downloads.remove(&key);
                }
                Effect::Task(Task::none())
            }
            Msg::MarkRead(index, finished) => {
                let key = if self.site.is_anime() {
                    self.episode_at(index).map(|e| episode_key(e.id))
                } else {
                    self.visible_chapters()
                        .get(index)
                        .map(|c| chapter_key(c.id, self.branch_of(c)))
                };
                if let Some(key) = key {
                    let p = Progress {
                        position: 0,
                        total: 0,
                        finished,
                    };
                    // Снять отметку можно только перезаписью: MAX в базе её бережёт.
                    // На сайте отметку снять нельзя, поэтому туда уходит только «прочитано».
                    let mut task = Task::none();
                    if finished {
                        let _ = ctx.db.save_progress(self.site, &self.slug, &key, p);
                        match self.view_mark(index) {
                            Some(mark) => ctx.sync.push(mark),
                            // Плееры серии ещё не загружены: без них сайт отметку не примет.
                            None if self.site.is_anime() && self.data.as_ref().is_some_and(|d| !d.offline) => {
                                if let Some(id) = self.episode_at(index).map(|e| e.id) {
                                    task = self.load_episode(ctx, id, EpisodeAction::MarkViewed);
                                }
                            }
                            None => {}
                        }
                    } else {
                        let _ = ctx.db.clear_progress(self.site, &self.slug, &key);
                    }
                    self.refresh_local(ctx);
                    return Effect::Task(task);
                }
                Effect::Task(Task::none())
            }
            Msg::Export => self.export(ctx),
            Msg::Enqueued(result, locked) => {
                self.refresh_local(ctx);
                match result {
                    Ok(added) if added + locked > 1 || locked > 0 => {
                        let mut text = format!("В очередь добавлено глав: {added}");
                        if locked > 0 {
                            text.push_str(&format!(", закрытых пропущено: {locked}"));
                        }
                        Effect::Toast(text)
                    }
                    Ok(_) => Effect::Task(Task::none()),
                    Err(e) => Effect::Toast(format!("Не поставилось в очередь: {e}")),
                }
            }
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
            Msg::Image(key, loaded) => Effect::Task(images.finish(key, loaded, Msg::Image)),
            Msg::Team(choice) => {
                self.team = choice.id;
                Effect::Task(Task::none())
            }
            Msg::EpisodeLoaded(id, action, result) => self.episode_loaded(ctx, id, action, result),
            Msg::Watch(i) => self.episode_action(ctx, i, EpisodeAction::Watch),
            Msg::DownloadEpisode(i) => self.episode_action(ctx, i, EpisodeAction::Download),
            Msg::Kodik(i) => self.episode_action(ctx, i, EpisodeAction::Kodik),
            Msg::Launched(result) => {
                self.busy = None;
                match result {
                    Ok(text) | Err(text) => Effect::Toast(text),
                }
            }
            Msg::PlaybackEnded => {
                self.refresh_local(ctx);
                Effect::Task(Task::none())
            }
        }
    }

    fn read(&mut self, ctx: &Ctx, index: usize) -> Effect {
        let Some(data) = &self.data else {
            return Effect::Task(Task::none());
        };
        let chapters: Vec<Chapter> = self.visible_chapters_asc().into_iter().cloned().collect();
        // Индекс приходит в порядке показа, читалке нужен порядок от первой главы.
        let shown = self.visible_chapters();
        let Some(target) = shown.get(index) else {
            return Effect::Task(Task::none());
        };
        let start = chapters.iter().position(|c| c.id == target.id).unwrap_or(0);
        let branches: HashMap<u64, Option<u64>> = chapters.iter().map(|c| (c.id, self.branch_of(c))).collect();
        let local: HashMap<u64, PathBuf> = chapters
            .iter()
            .filter_map(|c| {
                let key = chapter_key(c.id, branches[&c.id]);
                let row = self.downloads.get(&key)?;
                let dir = PathBuf::from(&row.path);
                library::chapter_complete(&dir).then_some((c.id, dir))
            })
            .collect();
        let _ = ctx.db.upsert_title(&title_row(self.site, &data.details));
        Effect::Read(ReaderStart {
            site: self.site,
            slug: self.slug.clone(),
            title_id: data.details.short.id,
            title: data.details.short.display_name().to_string(),
            chapters,
            branches,
            local,
            index: start,
            offline: data.offline,
        })
    }

    /// Ставит главы в очередь одной транзакцией в фоне: на сотнях глав запись в базу
    /// иначе подвесила бы окно.
    fn enqueue_chapters(&mut self, ctx: &Ctx, chapters: &[Chapter]) -> Effect {
        let Some(data) = &self.data else {
            return Effect::Task(Task::none());
        };
        let title = title_row(self.site, &data.details);
        let mut jobs = Vec::new();
        let mut locked = 0;
        for chapter in chapters {
            let branch = self.branch_of(chapter);
            if chapter.branch(branch).is_some_and(|b| b.is_open()) {
                jobs.push((chapter.title(), Job::chapter(chapter, branch)));
            } else {
                locked += 1;
            }
        }
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

    /// С какой главы продолжить: начатая и не дочитанная, иначе первая непрочитанная
    /// после последней прочитанной, иначе первая.
    fn continue_target(&self) -> Option<u64> {
        let chapters = self.visible_chapters_asc();
        let open = |c: &Chapter| c.branch(self.branch_of(c)).is_some_and(|b| b.is_open());
        if let Some(c) = chapters
            .iter()
            .find(|c| open(c) && self.progress_of(c).is_some_and(|p| !p.finished && p.position > 0))
        {
            return Some(c.id);
        }
        let last_read = chapters
            .iter()
            .rposition(|c| self.progress_of(c).is_some_and(|p| p.finished));
        let from = last_read.map(|i| i + 1).unwrap_or(0);
        chapters[from.min(chapters.len())..]
            .iter()
            .find(|c| open(c))
            .or_else(|| chapters.iter().find(|c| open(c)))
            .map(|c| c.id)
    }

    fn export(&mut self, ctx: &Ctx) -> Effect {
        let Some(data) = &self.data else {
            return Effect::Task(Task::none());
        };
        let mut by_volume: std::collections::BTreeMap<String, Vec<(f32, ExportChapter)>> = Default::default();
        for chapter in self.visible_chapters_asc() {
            let key = chapter_key(chapter.id, self.branch_of(chapter));
            let Some(row) = self.downloads.get(&key) else { continue };
            let dir = PathBuf::from(&row.path);
            if !library::chapter_complete(&dir) {
                continue;
            }
            by_volume.entry(chapter.volume.clone()).or_default().push((
                chapter.number_f32(),
                ExportChapter {
                    dir,
                    volume: chapter.volume.clone(),
                    number: chapter.number.clone(),
                },
            ));
        }
        if self.busy.is_some() {
            return Effect::Task(Task::none());
        }
        if by_volume.is_empty() {
            return Effect::Toast("Сначала скачайте главы: экспорт собирает CBZ из скачанного.".into());
        }
        let name = data.details.short.display_name().to_string();
        let out_dir = ctx.library.root().join("export").join(library::safe(&name));
        self.busy = Some("Собираю CBZ…".into());
        let task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    for (volume, mut chapters) in by_volume {
                        chapters.sort_by(|a, b| a.0.total_cmp(&b.0));
                        let chapters: Vec<ExportChapter> = chapters.into_iter().map(|(_, c)| c).collect();
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
        );
        Effect::Task(task)
    }

    fn load_episode(&self, ctx: &Ctx, id: u64, action: EpisodeAction) -> Task<Msg> {
        let client = ctx.client.clone();
        Task::perform(
            async move { client.episode(id).await.map_err(|e| human_error(&e)) },
            move |r| Msg::EpisodeLoaded(id, action, r),
        )
    }

    fn episode_action(&mut self, ctx: &Ctx, index: usize, action: EpisodeAction) -> Effect {
        let Some(episode) = self.episode_at(index).cloned() else {
            return Effect::Task(Task::none());
        };
        // Скачанную серию смотрим с диска, сеть не нужна.
        if matches!(action, EpisodeAction::Watch)
            && let Some(row) = self.downloads.get(&episode_key(episode.id))
            && row.state == DownloadState::Done
        {
            let path = PathBuf::from(&row.path);
            let player_id = match serde_json::from_str::<Job>(&row.job) {
                Ok(Job::Episode { player_id, .. }) => Some(player_id),
                _ => None,
            };
            let player = player_id.and_then(|id| {
                self.episodes
                    .get(&episode.id)
                    .and_then(|d| d.players.iter().find(|p| p.id == id))
                    .cloned()
            });
            let start = self.resume_at(episode.id);
            self.mark_started(ctx, episode.id);
            let mut track = self.track(ctx, &episode, player.as_ref());
            track.player_id = track.player_id.or(player_id);
            return Effect::Task(watch_player(
                polka_core::player::play_file(path, start, episode.title()),
                track,
            ));
        }
        match self.episodes.get(&episode.id).cloned() {
            Some(details) => self.run_episode_action(ctx, &episode, &details, action),
            None => {
                self.busy = Some("Загружаю серию…".into());
                Effect::Task(self.load_episode(ctx, episode.id, action))
            }
        }
    }

    fn episode_loaded(
        &mut self,
        ctx: &Ctx,
        id: u64,
        action: EpisodeAction,
        result: Result<EpisodeDetails, String>,
    ) -> Effect {
        // Ответ мог прийти от серии другого тайтла, если страницу успели сменить.
        let ours = self
            .data
            .as_ref()
            .is_some_and(|d| d.episodes.iter().any(|e| e.id == id));
        if !ours {
            return Effect::Task(Task::none());
        }
        self.busy = None;
        let details = match result {
            Ok(d) => d,
            Err(e) => return Effect::Toast(format!("Серия не загрузилась: {e}")),
        };
        self.episodes.insert(id, details.clone());
        if matches!(action, EpisodeAction::Teams) {
            self.teams = details
                .players
                .iter()
                .filter(|p| p.kind() == PlayerKind::Animelib)
                .map(|p| Choice {
                    id: p.team.as_ref().map(|t| t.id),
                    label: format!("{} ({})", p.team_name(), translation(p)),
                })
                .collect();
            self.teams.dedup_by(|a, b| a.id == b.id);
            if self.team.is_none() {
                self.team = self.teams.first().and_then(|c| c.id);
            }
            return Effect::Task(Task::none());
        }
        let Some(episode) = self
            .data
            .as_ref()
            .and_then(|d| d.episodes.iter().find(|e| e.id == id))
            .cloned()
        else {
            return Effect::Task(Task::none());
        };
        self.run_episode_action(ctx, &episode, &details, action)
    }

    fn run_episode_action(
        &mut self,
        ctx: &Ctx,
        episode: &Episode,
        details: &EpisodeDetails,
        action: EpisodeAction,
    ) -> Effect {
        let own: Vec<_> = details
            .players
            .iter()
            .filter(|p| p.kind() == PlayerKind::Animelib)
            .collect();
        let player = own
            .iter()
            .find(|p| p.team.as_ref().map(|t| t.id) == self.team)
            .or(own.first())
            .copied();
        match action {
            EpisodeAction::Teams => Effect::Task(Task::none()),
            EpisodeAction::MarkViewed => {
                let any = details
                    .players
                    .iter()
                    .find(|p| p.team.as_ref().map(|t| t.id) == self.team)
                    .or_else(|| details.players.first());
                if let (Some(player), Some(data)) = (player.or(any), &self.data) {
                    ctx.sync.push(Mark::EpisodeViewed {
                        anime_id: data.details.short.id,
                        player_id: player.id,
                    });
                }
                Effect::Task(Task::none())
            }
            EpisodeAction::Kodik => {
                let kodik = details
                    .players
                    .iter()
                    .find(|p| p.kind() == PlayerKind::Kodik && p.team.as_ref().map(|t| t.id) == self.team)
                    .or_else(|| details.players.iter().find(|p| p.kind() == PlayerKind::Kodik));
                // Открываем в браузере только https: адрес пришёл с сервера.
                match kodik.and_then(|p| p.embed_url()).filter(|u| u.starts_with("https://")) {
                    Some(url) => {
                        let _ = open::that_detached(url);
                        Effect::Task(Task::none())
                    }
                    None => Effect::Toast("У этой серии нет плеера Kodik".into()),
                }
            }
            EpisodeAction::Watch => {
                let Some(player) = player.cloned() else {
                    return Effect::Toast(if ctx_logged_hint(details) {
                        "Своего плеера AnimeLib у серии нет. Можно открыть Kodik в браузере.".into()
                    } else {
                        "Видео AnimeLib доступно после входа (Настройки → Войти).".into()
                    });
                };
                let start = self.resume_at(episode.id);
                self.mark_started(ctx, episode.id);
                let track = self.track(ctx, episode, Some(&player));
                self.busy = Some("Запускаю плеер…".into());
                let client = ctx.client.clone();
                let quality = ctx.config.max_quality;
                let title = format!(
                    "{} – {}",
                    self.data.as_ref().map(|d| d.details.short.display_name()).unwrap_or(""),
                    episode.title()
                );
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
                let Some(player) = player else {
                    return Effect::Toast("Скачать можно только серии с плеером AnimeLib".into());
                };
                let Some(data) = &self.data else {
                    return Effect::Task(Task::none());
                };
                let title = title_row(self.site, &data.details);
                let label = format!("{} ({})", episode.title(), player.team_name());
                match ctx
                    .downloader
                    .enqueue(&title, &label, Job::episode(episode, player, ctx.config.max_quality))
                {
                    Ok(_) => {
                        self.refresh_local(ctx);
                        Effect::Task(Task::none())
                    }
                    Err(e) => Effect::Toast(format!("Не поставилось в очередь: {e:#}")),
                }
            }
        }
    }

    // --- Помощники ---

    /// С какой секунды продолжить серию. Досмотренную открываем с начала.
    fn resume_at(&self, episode_id: u64) -> Option<u64> {
        self.progress
            .get(&episode_key(episode_id))
            .filter(|p| !p.finished)
            .map(|p| p.position)
    }

    fn track(&self, ctx: &Ctx, episode: &Episode, player: Option<&Player>) -> polka_core::player::Track {
        polka_core::player::Track {
            db: ctx.db.clone(),
            sync: ctx.sync.clone(),
            site: self.site,
            slug: self.slug.clone(),
            key: episode_key(episode.id),
            anime_id: self.data.as_ref().map_or(0, |d| d.details.short.id),
            episode_id: episode.id,
            item_number: episode.item_number,
            player_id: player.map(|p| p.id),
            choice: player.and_then(Player::choice),
        }
    }

    /// Пришли данные аккаунта: если страницу открыли раньше, догружаем закладку.
    pub fn on_account(&self, ctx: &Ctx) -> Task<Msg> {
        match &self.data {
            Some(data) if !data.offline && !self.bookmark_known => self.load_bookmark(ctx),
            _ => Task::none(),
        }
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

    /// Кладёт тайтл в папку на сайте или убирает из закладок.
    fn set_folder(&mut self, ctx: &Ctx, folder: Option<u64>) -> Effect {
        if self.busy.is_some() {
            return Effect::Task(Task::none());
        }
        let (site, slug, client) = (self.site, self.slug.clone(), ctx.client.clone());
        let current = self.bookmark.as_ref().map(|b| b.id);
        if let Some(data) = &self.data {
            let _ = ctx.db.upsert_title(&title_row(site, &data.details));
        }
        self.busy = Some("Сохраняю закладку…".into());
        Effect::Task(Task::perform(
            {
                let slug = slug.clone();
                async move {
                    let result = match (folder, current) {
                        (Some(folder), _) => client.set_bookmark(site, &slug, folder, None).await.map(Some),
                        (None, Some(id)) => client.delete_bookmark(site, id).await.map(|()| None),
                        (None, None) => Ok(None),
                    };
                    result.map(|b| b.map(Box::new)).map_err(|e| human_error(&e))
                }
            },
            move |r| Msg::BookmarkSaved(site, slug.clone(), r),
        ))
    }

    fn set_local_library(&mut self, ctx: &Ctx, value: bool) {
        if ctx.db.set_in_library(self.site, &self.slug, value).is_ok() {
            self.in_library = value;
        }
    }

    /// Подпись кнопки «Продолжить» по закладке, если в ней записано место.
    fn resume_label(&self) -> Option<String> {
        let bookmark = self.bookmark.as_ref()?;
        let item_id = bookmark.item_id?;
        let progress = bookmark.progress.as_deref().unwrap_or_default();
        if self.site.is_anime() {
            let episode = self.data.as_ref()?.episodes.iter().find(|e| e.id == item_id)?;
            Some(match parse_timecode(progress).filter(|s| *s > 0) {
                Some(secs) => format!("Продолжить: {}, {}", episode.title(), format_timecode(secs)),
                None => format!("Продолжить: {}", episode.title()),
            })
        } else {
            let target = self.manga_resume()?;
            let chapter = self
                .data
                .as_ref()?
                .chapters
                .iter()
                .find(|c| c.id == target.chapter_id)?;
            Some(match target.page {
                Some(page) => format!(
                    "Продолжить: том {} глава {}, стр. {page}",
                    chapter.volume, chapter.number
                ),
                None => format!("Продолжить: том {} глава {}", chapter.volume, chapter.number),
            })
        }
    }

    /// Куда ведёт «Продолжить» у манги. Место из закладки уточняется своим: если
    /// главу здесь уже дочитали, открывается следующая, а начатая продолжается со
    /// своей страницы. Подпись кнопки и сам переход считаются здесь же, чтобы
    /// кнопка не обещала одно, а открывала другое.
    fn manga_resume(&self) -> Option<MangaResume> {
        let bookmark = self.bookmark.as_ref()?;
        let (chapter, bookmark_branch) = self.find_chapter(bookmark.item_id?)?;
        // Ветку закладки берём, только если у тайтла выбор веток, как в списке глав.
        let branch = if self.branch.is_some() && bookmark_branch.is_some() {
            bookmark_branch
        } else {
            self.branch_of(chapter)
        };
        let resume = |chapter_id, page| MangaResume {
            chapter_id,
            branch,
            page,
            from_bookmark: false,
        };
        match self.progress.get(&chapter_key(chapter.id, branch)) {
            Some(p) if p.finished => {
                let list = self.visible_chapters_asc();
                let at = list.iter().position(|c| c.id == chapter.id)?;
                let next = list[at + 1..]
                    .iter()
                    .find(|c| c.branch(self.branch_of(c)).is_some_and(|b| b.is_open()))?;
                Some(resume(next.id, None))
            }
            Some(p) if p.position > 0 => Some(resume(chapter.id, Some(p.position + 1))),
            _ => {
                let page = bookmark.progress.as_deref().and_then(|p| p.parse::<u64>().ok());
                Some(MangaResume {
                    from_bookmark: true,
                    ..resume(chapter.id, page.filter(|p| *p > 1))
                })
            }
        }
    }

    /// Глава по id перевода из закладки и ветка этого перевода.
    fn find_chapter(&self, item_id: u64) -> Option<(&Chapter, Option<u64>)> {
        let data = self.data.as_ref()?;
        data.chapters
            .iter()
            .find_map(|c| match c.branches.iter().find(|b| b.id == item_id) {
                Some(b) => Some((c, b.branch_id)),
                None => (c.id == item_id).then_some((c, None)),
            })
    }

    /// Открывает главу или серию из закладки с записанного места. Своё место на
    /// этом устройстве важнее: его не перетираем.
    fn resume(&mut self, ctx: &Ctx) -> Effect {
        let Some(bookmark) = self.bookmark.clone() else {
            return Effect::Task(Task::none());
        };
        let Some(item_id) = bookmark.item_id else {
            return Effect::Task(Task::none());
        };
        let progress = bookmark.progress.as_deref().unwrap_or_default();
        if self.site.is_anime() {
            let Some(index) = self.episodes_shown().iter().position(|e| e.id == item_id) else {
                return Effect::Task(Task::none());
            };
            if let Some(team) = bookmark.meta.team
                && self.teams.iter().any(|c| c.id == Some(team))
            {
                self.team = Some(team);
            }
            let key = episode_key(item_id);
            if let Some(secs) = parse_timecode(progress)
                && !self.progress.contains_key(&key)
            {
                let p = Progress {
                    position: secs,
                    total: 0,
                    finished: false,
                };
                let _ = ctx.db.save_progress(self.site, &self.slug, &key, p);
                self.progress.insert(key, p);
            }
            return self.episode_action(ctx, index, EpisodeAction::Watch);
        }
        let Some(target) = self.manga_resume() else {
            return Effect::Task(Task::none());
        };
        // Глава из другой ветки перевода: переключаемся на неё, как сайт.
        if target.branch.is_some() && self.branch.is_some() && self.branch != target.branch {
            self.branch = target.branch;
        }
        let shown = self.visible_chapters();
        let Some(index) = shown.iter().position(|c| c.id == target.chapter_id) else {
            return Effect::Task(Task::none());
        };
        // Страницу из закладки читалка возьмёт из базы, как своё место.
        if target.from_bookmark
            && let Some(page) = target.page
        {
            let key = chapter_key(target.chapter_id, self.branch_of(shown[index]));
            let p = Progress {
                position: page - 1,
                total: 0,
                finished: false,
            };
            let _ = ctx.db.save_progress(self.site, &self.slug, &key, p);
            self.progress.insert(key, p);
        }
        self.read(ctx, index)
    }

    /// Отметка «прочитано» или «просмотрено» для сайта по строке списка.
    fn view_mark(&self, index: usize) -> Option<Mark> {
        let data = self.data.as_ref()?;
        if self.site.is_anime() {
            let episode = self.episode_at(index)?;
            let details = self.episodes.get(&episode.id)?;
            let player = details
                .players
                .iter()
                .filter(|p| p.kind() == PlayerKind::Animelib)
                .find(|p| p.team.as_ref().map(|t| t.id) == self.team)
                .or_else(|| details.players.first())?;
            return Some(Mark::EpisodeViewed {
                anime_id: data.details.short.id,
                player_id: player.id,
            });
        }
        let chapter = *self.visible_chapters().get(index)?;
        let chapter_id = chapter.branch(self.branch_of(chapter)).map_or(chapter.id, |b| b.id);
        Some(Mark::ChapterViewed {
            site: self.site,
            title_id: data.details.short.id,
            chapter_id,
        })
    }

    /// Отмечает серию начатой, не затирая уже записанный прогресс.
    fn mark_started(&mut self, ctx: &Ctx, episode_id: u64) {
        let key = episode_key(episode_id);
        if self.progress.contains_key(&key) {
            return;
        }
        let p = Progress {
            position: 0,
            total: 0,
            finished: false,
        };
        let _ = ctx.db.save_progress(self.site, &self.slug, &key, p);
        self.progress.insert(key, p);
    }

    fn branch_of(&self, chapter: &Chapter) -> Option<u64> {
        pick_branch(chapter, self.branch)
    }

    fn progress_of(&self, chapter: &Chapter) -> Option<&Progress> {
        self.progress.get(&chapter_key(chapter.id, self.branch_of(chapter)))
    }

    /// Главы выбранной ветки от первой к последней.
    fn visible_chapters_asc(&self) -> Vec<&Chapter> {
        let Some(data) = &self.data else { return Vec::new() };
        let multi = has_multi(&data.chapters);
        data.chapters
            .iter()
            .filter(|c| !multi || self.branch.is_none() || c.branches.iter().any(|b| b.branch_id == self.branch))
            .collect()
    }

    /// Главы в порядке показа.
    fn visible_chapters(&self) -> Vec<&Chapter> {
        let mut list = self.visible_chapters_asc();
        if self.newest_first {
            list.reverse();
        }
        list
    }

    fn episodes_shown(&self) -> Vec<&Episode> {
        let Some(data) = &self.data else { return Vec::new() };
        let mut list: Vec<&Episode> = data.episodes.iter().collect();
        if self.newest_first {
            list.reverse();
        }
        list
    }

    fn episode_at(&self, index: usize) -> Option<&Episode> {
        self.episodes_shown().get(index).copied()
    }

    // --- Вид ---

    pub fn view<'a>(&'a self, ctx: &'a Ctx, images: &'a Images) -> Element<'a, Msg> {
        let back = button("← Назад").style(button::text).on_press(Msg::Back);
        let Some(data) = &self.data else {
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
        let library: Element<'a, Msg> = if self.bookmark_known && !folders.is_empty() && !data.offline {
            // С входом библиотека – это закладки сайта с его папками.
            let mut choices: Vec<Choice> = folders
                .iter()
                .map(|f| Choice {
                    id: Some(f.id),
                    label: f.name.clone(),
                })
                .collect();
            let selected = self
                .bookmark
                .as_ref()
                .and_then(|b| choices.iter().find(|c| c.id == Some(b.status)))
                .cloned();
            if self.bookmark.is_some() {
                choices.push(Choice {
                    id: None,
                    label: "Убрать из закладок".into(),
                });
            }
            pick_list(choices, selected, Msg::Folder)
                .placeholder("+ В закладки")
                .into()
        } else {
            button(if self.in_library {
                "✓ В библиотеке"
            } else {
                "+ В библиотеку"
            })
            .style(if self.in_library {
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
        let resume = self.resume_label();
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

        let list: Element<'a, Msg> = if self.site.is_anime() {
            self.episodes_view()
        } else {
            self.chapters_view()
        };

        scrollable(column![header, summary, list].spacing(18).padding(20).max_width(1100))
            .height(Fill)
            .width(Fill)
            .into()
    }

    fn chapters_view(&self) -> Element<'_, Msg> {
        let Some(data) = &self.data else { return text("").into() };
        let mut controls = row![
            text(format!("Глав: {}", self.visible_chapters_asc().len())).size(16),
            button(if self.newest_first {
                "Сначала новые"
            } else {
                "Сначала первые"
            })
            .style(button::text)
            .on_press(Msg::Order),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if has_multi(&data.chapters) && !data.branches.is_empty() {
            let choices: Vec<Choice> = data
                .branches
                .iter()
                .map(|b| Choice {
                    id: Some(b.id),
                    label: b
                        .teams
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                        .chars()
                        .take(60)
                        .collect::<String>()
                        .if_empty(|| format!("Перевод {}", b.id)),
                })
                .collect();
            let selected = choices.iter().find(|c| c.id == self.branch).cloned();
            controls = controls
                .push(text("Перевод:").size(14))
                .push(pick_list(choices, selected, Msg::Branch));
        }

        let rows = self.visible_chapters().into_iter().enumerate().map(|(i, c)| {
            let branch = self.branch_of(c);
            let info = c.branch(branch);
            let open = info.is_some_and(|b| b.is_open());
            let key = chapter_key(c.id, branch);
            let mut status = Vec::new();
            if !open {
                let until = info
                    .and_then(|b| b.restricted_view.as_ref())
                    .and_then(|r| r.expired_at.as_deref())
                    .map(short_date);
                status.push(match until {
                    Some(d) => format!("🔒 откроется {d}"),
                    None => "🔒 платная".into(),
                });
            }
            match self.progress.get(&key) {
                Some(p) if p.finished => status.push("✓ прочитано".into()),
                Some(p) if p.total > 0 => status.push(format!("стр. {}/{}", p.position + 1, p.total)),
                _ => {}
            }
            let dl = self.downloads.get(&key);
            if let Some(row) = dl {
                status.push(download_label(row));
            }
            let read_done = self.progress.get(&key).is_some_and(|p| p.finished);
            let downloaded = dl.is_some();
            row![
                column![
                    text(c.title()).size(15),
                    text(info.and_then(|b| b.translator()).unwrap_or("").to_string())
                        .size(12)
                        .style(text::secondary),
                ]
                .width(Fill),
                text(status.join("  ")).size(13).style(text::secondary),
                button(if read_done { "↺" } else { "✓" })
                    .style(button::text)
                    .on_press(Msg::MarkRead(i, !read_done)),
                button("Читать").on_press_maybe(open.then_some(Msg::Read(i))),
                if downloaded {
                    button("Удалить").style(button::danger).on_press(Msg::Remove(i))
                } else {
                    button("Скачать")
                        .style(button::secondary)
                        .on_press_maybe(open.then_some(Msg::Download(i)))
                },
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .padding([6, 10])
            .into()
        });
        column![controls, column(rows).spacing(2)].spacing(10).into()
    }

    fn episodes_view(&self) -> Element<'_, Msg> {
        let mut controls = row![
            text(format!("Серий: {}", self.episodes_shown().len())).size(16),
            button(if self.newest_first {
                "Сначала новые"
            } else {
                "Сначала первые"
            })
            .style(button::text)
            .on_press(Msg::Order),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if !self.teams.is_empty() {
            let selected = self.teams.iter().find(|c| c.id == self.team).cloned();
            controls =
                controls
                    .push(text("Озвучка:").size(14))
                    .push(pick_list(self.teams.clone(), selected, Msg::Team));
        }
        let rows = self.episodes_shown().into_iter().enumerate().map(|(i, e)| {
            let key = episode_key(e.id);
            let mut status = Vec::new();
            if let Some(p) = self.progress.get(&key) {
                status.push(if p.finished {
                    "✓ просмотрено".to_string()
                } else if p.position > 0 {
                    format!("остановились на {}", clock(p.position))
                } else {
                    "начато".to_string()
                });
            }
            let dl = self.downloads.get(&key);
            if let Some(row) = dl {
                status.push(download_label(row));
            }
            let watched = self.progress.get(&key).is_some_and(|p| p.finished);
            row![
                text(e.title()).size(15).width(Fill),
                text(status.join("  ")).size(13).style(text::secondary),
                button(if watched { "↺" } else { "✓" })
                    .style(button::text)
                    .on_press(Msg::MarkRead(i, !watched)),
                button("Смотреть").on_press(Msg::Watch(i)),
                button("Kodik").style(button::text).on_press(Msg::Kodik(i)),
                if dl.is_some() {
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

fn has_multi(chapters: &[Chapter]) -> bool {
    chapters.iter().any(|c| c.branches.len() > 1)
}

fn translation(p: &libsocial::models::Player) -> String {
    p.translation_type
        .as_ref()
        .map(|t| t.label.to_lowercase())
        .unwrap_or_default()
}

/// Подсказка для серии без своего плеера: видно ли, что вход уже выполнен.
fn ctx_logged_hint(details: &EpisodeDetails) -> bool {
    details.players.iter().any(|p| p.kind() == PlayerKind::Kodik)
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

trait IfEmpty {
    fn if_empty(self, f: impl FnOnce() -> String) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, f: impl FnOnce() -> String) -> String {
        if self.trim().is_empty() { f() } else { self }
    }
}

/// Загружает тайтл из сети, а без сети собирает его из библиотеки на диске.
async fn load_title(ctx: Ctx, site: Site, slug: String) -> Result<TitleData, String> {
    let online = async {
        let details = ctx.client.title(site, &slug).await?;
        let (chapters, branches, episodes) = if site.is_anime() {
            (Vec::new(), Vec::new(), ctx.client.episodes(&slug).await?)
        } else {
            let chapters = ctx.client.chapters(site, &slug).await?;
            let branches = if has_multi(&chapters) {
                ctx.client.branches(site, details.short.id).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            (chapters, branches, Vec::new())
        };
        Ok::<_, libsocial::Error>(TitleData {
            details,
            chapters,
            branches,
            episodes,
            offline: false,
        })
    };
    match online.await {
        Ok(data) => Ok(data),
        // Скачанное показываем только когда до сервера не достучаться, он лежит или
        // просит подождать. На 404 или испорченный ответ честно показываем ошибку.
        Err(
            e @ (libsocial::Error::Network(_)
            | libsocial::Error::Forbidden
            | libsocial::Error::ServiceDown { .. }
            | libsocial::Error::RateLimited { .. }),
        ) => offline_title(&ctx, site, &slug).ok_or_else(|| human_error(&e)),
        Err(e) => Err(human_error(&e)),
    }
}

fn offline_title(ctx: &Ctx, site: Site, slug: &str) -> Option<TitleData> {
    let row = ctx.db.title(site, slug).ok()??;
    let details: TitleDetails = serde_json::from_str(row.details.as_deref()?).ok()?;
    let downloads = ctx.db.downloads_for(site, slug).ok()?;
    let mut chapters = Vec::new();
    let mut episodes = Vec::new();
    for d in downloads.iter().filter(|d| d.state == DownloadState::Done) {
        let Ok(job) = serde_json::from_str::<Job>(&d.job) else {
            continue;
        };
        match job {
            Job::Chapter {
                chapter_id,
                volume,
                number,
                branch_id,
            } => {
                // Две скачанные ветки одной главы показываем одной строкой.
                if chapters.iter().any(|c: &Chapter| c.id == chapter_id) {
                    continue;
                }
                let Ok(chapter) = serde_json::from_value::<Chapter>(serde_json::json!({
                    "id": chapter_id, "volume": volume, "number": number,
                    "branches_count": 1,
                    "branches": [{"id": chapter_id, "branch_id": branch_id, "teams": []}],
                })) else {
                    continue;
                };
                chapters.push(chapter);
            }
            Job::Episode {
                episode_id,
                season,
                number,
                ..
            } => {
                let Ok(ep) = serde_json::from_value::<Episode>(serde_json::json!({
                    "id": episode_id, "season": season, "number": number,
                })) else {
                    continue;
                };
                episodes.push(ep);
            }
        }
    }
    chapters.sort_by(|a, b| {
        (a.volume.parse::<f32>().unwrap_or(0.0), a.number_f32())
            .partial_cmp(&(b.volume.parse::<f32>().unwrap_or(0.0), b.number_f32()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let _ = mirrors::web(site);
    Some(TitleData {
        details,
        chapters,
        branches: Vec::new(),
        episodes,
        offline: true,
    })
}
