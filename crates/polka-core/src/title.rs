//! Страница тайтла без интерфейса: загрузка с запасным вариантом без сети, ветки
//! перевода, состояние глав и серий, «Продолжить», отметки для сайта, задания
//! загрузчика и экспорт. Интерфейс только рисует то, что здесь посчитано.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use libsocial::bookmarks::parse_timecode;
use libsocial::models::{Branch, Chapter, ChapterBranch, Episode, EpisodeDetails, Player, PlayerKind, TitleDetails};
use libsocial::{Bookmark, Site};

use crate::context::Core;
use crate::db::{DownloadRow, DownloadState, Progress};
use crate::downloads::DownloadEvent;
use crate::library::{self, ExportChapter, Job, chapter_key, episode_key};
use crate::player::Track;
use crate::reader::ReaderStart;
use crate::sync::Mark;
use crate::titles::{pick_branch, title_row};

/// Всё, что сервер знает о тайтле.
#[derive(Debug, Clone)]
pub struct TitleData {
    pub details: TitleDetails,
    pub chapters: Vec<Chapter>,
    pub branches: Vec<Branch>,
    pub episodes: Vec<Episode>,
    /// Данные собраны из библиотеки на диске, сеть недоступна.
    pub offline: bool,
}

/// Загружает тайтл из сети, а без сети собирает его из библиотеки на диске.
/// Скачанное показывается, только когда до сервера не достучаться, он лежит или
/// просит подождать. На 404 или испорченный ответ возвращается ошибка.
pub async fn load(core: &Core, site: Site, slug: &str) -> libsocial::Result<TitleData> {
    let online = async {
        let details = core.client.title(site, slug).await?;
        let (chapters, branches, episodes) = if site.is_anime() {
            (Vec::new(), Vec::new(), core.client.episodes(slug).await?)
        } else {
            let chapters = core.client.chapters(site, slug).await?;
            let branches = if has_multi(&chapters) {
                core.client.branches(site, details.short.id).await.unwrap_or_default()
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
        Ok(data) => {
            let _ = core.db.upsert_title(&title_row(site, &data.details));
            Ok(data)
        }
        Err(
            e @ (libsocial::Error::Network(_)
            | libsocial::Error::Forbidden
            | libsocial::Error::ServiceDown { .. }
            | libsocial::Error::RateLimited { .. }),
        ) => offline(core, site, slug).ok_or(e),
        Err(e) => Err(e),
    }
}

/// Тайтл из библиотеки на диске: карточка из базы и скачанные главы или серии.
pub fn offline(core: &Core, site: Site, slug: &str) -> Option<TitleData> {
    let row = core.db.title(site, slug).ok()??;
    let details: TitleDetails = serde_json::from_str(row.details.as_deref()?).ok()?;
    let downloads = core.db.downloads_for(site, slug).ok()?;
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
                item_id,
            } => {
                // Две скачанные ветки одной главы показываем одной строкой.
                if chapters.iter().any(|c: &Chapter| c.id == chapter_id) {
                    continue;
                }
                let Ok(chapter) = serde_json::from_value::<Chapter>(serde_json::json!({
                    "id": chapter_id, "volume": volume, "number": number,
                    "branches_count": 1,
                    "branches": [{"id": item_id.unwrap_or(chapter_id), "branch_id": branch_id, "teams": []}],
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
    Some(TitleData {
        details,
        chapters,
        branches: Vec::new(),
        episodes,
        offline: true,
    })
}

/// У тайтла есть главы в нескольких переводах.
pub fn has_multi(chapters: &[Chapter]) -> bool {
    chapters.iter().any(|c| c.branches.len() > 1)
}

/// Насколько прочитана глава или просмотрена серия на этом устройстве.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    None,
    /// Начато: страница с нуля или секунда, и сколько всего (0, если неизвестно).
    Started {
        position: u64,
        total: u64,
    },
    Finished,
}

impl Read {
    fn of(progress: Option<&Progress>) -> Read {
        match progress {
            Some(p) if p.finished => Read::Finished,
            Some(p) => Read::Started {
                position: p.position,
                total: p.total,
            },
            None => Read::None,
        }
    }
}

/// Строка списка глав со всем, что о ней известно.
pub struct ChapterRow<'a> {
    pub chapter: &'a Chapter,
    pub branch: Option<u64>,
    /// Перевод главы в выбранной ветке.
    pub info: Option<&'a ChapterBranch>,
    pub open: bool,
    /// Когда закрытая глава откроется бесплатно (ранний доступ), если известно.
    pub opens_at: Option<&'a str>,
    pub read: Read,
    pub download: Option<&'a DownloadRow>,
}

pub struct EpisodeRow<'a> {
    pub episode: &'a Episode,
    pub read: Read,
    pub download: Option<&'a DownloadRow>,
}

/// Куда ведёт «Продолжить» по закладке на сайте.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    Chapter {
        /// Id главы в списке (`Chapter::id`).
        chapter_id: u64,
        branch: Option<u64>,
        /// Страница с единицы. `None` – с начала главы.
        page: Option<u64>,
        /// Страница взята из закладки: своего места на этом устройстве нет.
        from_bookmark: bool,
    },
    Episode {
        episode_id: u64,
        /// Секунда, с которой продолжить.
        secs: Option<u64>,
        /// Команда озвучки из закладки.
        team: Option<u64>,
    },
}

/// Озвучка или перевод серии, из которых можно выбрать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub id: u64,
    pub name: String,
    /// «озвучка», «субтитры».
    pub translation: String,
}

/// Состояние страницы тайтла после загрузки.
pub struct TitlePage {
    pub site: Site,
    pub slug: String,
    pub data: TitleData,
    /// Выбранная ветка перевода. `None`, если у тайтла одна ветка.
    pub branch: Option<u64>,
    pub newest_first: bool,
    /// Загрузки по ключу элемента.
    pub downloads: HashMap<String, DownloadRow>,
    /// Прогресс по ключу элемента.
    pub progress: HashMap<String, Progress>,
    /// Тайтл в библиотеке на устройстве.
    pub in_library: bool,
    /// Закладка на сайте. `None` – тайтла нет в закладках или ответ ещё не пришёл.
    pub bookmark: Option<Bookmark>,
    /// Ответ о закладке получен.
    pub bookmark_known: bool,
    /// Аниме: команды с собственным плеером и выбранная из них.
    pub teams: Vec<Team>,
    pub team: Option<u64>,
    /// Загруженные серии со списком плееров.
    pub episodes: HashMap<u64, EpisodeDetails>,
}

impl TitlePage {
    pub fn new(core: &Core, site: Site, slug: String, data: TitleData) -> TitlePage {
        let branch = data
            .branches
            .first()
            .map(|b| b.id)
            .filter(|_| has_multi(&data.chapters));
        let mut page = TitlePage {
            site,
            slug,
            data,
            branch,
            newest_first: true,
            downloads: HashMap::new(),
            progress: HashMap::new(),
            in_library: false,
            bookmark: None,
            bookmark_known: false,
            teams: Vec::new(),
            team: None,
            episodes: HashMap::new(),
        };
        page.refresh_local(core);
        page
    }

    /// Перечитывает из базы загрузки, прогресс и флаг библиотеки.
    pub fn refresh_local(&mut self, core: &Core) {
        if let Ok(rows) = core.db.downloads_for(self.site, &self.slug) {
            self.downloads = rows.into_iter().map(|r| (r.item.clone(), r)).collect();
        }
        if let Ok(progress) = core.db.progress_for(self.site, &self.slug) {
            self.progress = progress.into_iter().collect();
        }
        self.in_library = core
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

    pub fn title_id(&self) -> u64 {
        self.data.details.short.id
    }

    // --- Главы ---

    pub fn has_multi(&self) -> bool {
        has_multi(&self.data.chapters)
    }

    pub fn branch_of(&self, chapter: &Chapter) -> Option<u64> {
        pick_branch(chapter, self.branch)
    }

    pub fn chapter_key_of(&self, chapter: &Chapter) -> String {
        chapter_key(chapter.id, self.branch_of(chapter))
    }

    fn progress_of(&self, chapter: &Chapter) -> Option<&Progress> {
        self.progress.get(&self.chapter_key_of(chapter))
    }

    fn is_open(&self, chapter: &Chapter) -> bool {
        chapter.branch(self.branch_of(chapter)).is_some_and(|b| b.is_open())
    }

    /// Ветки перевода для выбора: id и подпись из названий команд.
    pub fn branch_choices(&self) -> Vec<(u64, String)> {
        if !self.has_multi() {
            return Vec::new();
        }
        self.data
            .branches
            .iter()
            .map(|b| {
                let teams: Vec<&str> = b.teams.iter().map(|t| t.name.as_str()).collect();
                let label: String = teams.join(", ").chars().take(60).collect();
                let label = if label.trim().is_empty() {
                    format!("Перевод {}", b.id)
                } else {
                    label
                };
                (b.id, label)
            })
            .collect()
    }

    /// Главы выбранной ветки от первой к последней.
    pub fn chapters_asc(&self) -> Vec<&Chapter> {
        let multi = self.has_multi();
        self.data
            .chapters
            .iter()
            .filter(|c| !multi || self.branch.is_none() || c.branches.iter().any(|b| b.branch_id == self.branch))
            .collect()
    }

    /// Главы в порядке показа.
    pub fn chapters(&self) -> Vec<&Chapter> {
        let mut list = self.chapters_asc();
        if self.newest_first {
            list.reverse();
        }
        list
    }

    pub fn chapter_at(&self, index: usize) -> Option<&Chapter> {
        self.chapters().get(index).copied()
    }

    /// Строки списка глав в порядке показа.
    pub fn chapter_rows(&self) -> Vec<ChapterRow<'_>> {
        self.chapters()
            .into_iter()
            .map(|chapter| {
                let branch = self.branch_of(chapter);
                let info = chapter.branch(branch);
                let key = chapter_key(chapter.id, branch);
                ChapterRow {
                    chapter,
                    branch,
                    info,
                    open: info.is_some_and(|b| b.is_open()),
                    opens_at: info
                        .and_then(|b| b.restricted_view.as_ref())
                        .and_then(|r| r.expired_at.as_deref()),
                    read: Read::of(self.progress.get(&key)),
                    download: self.downloads.get(&key),
                }
            })
            .collect()
    }

    /// С какой главы читать без закладки: начатая и не дочитанная, иначе первая
    /// непрочитанная после последней прочитанной, иначе первая открытая.
    pub fn continue_target(&self) -> Option<u64> {
        let chapters = self.chapters_asc();
        if let Some(c) = chapters
            .iter()
            .find(|c| self.is_open(c) && self.progress_of(c).is_some_and(|p| !p.finished && p.position > 0))
        {
            return Some(c.id);
        }
        let last_read = chapters
            .iter()
            .rposition(|c| self.progress_of(c).is_some_and(|p| p.finished));
        let from = last_read.map(|i| i + 1).unwrap_or(0);
        chapters[from.min(chapters.len())..]
            .iter()
            .find(|c| self.is_open(c))
            .or_else(|| chapters.iter().find(|c| self.is_open(c)))
            .map(|c| c.id)
    }

    /// Индекс главы в порядке показа по её id.
    pub fn index_of_chapter(&self, chapter_id: u64) -> Option<usize> {
        self.chapters().iter().position(|c| c.id == chapter_id)
    }

    /// Всё, что нужно читалке, чтобы открыть главу с этим индексом в порядке показа.
    pub fn reader_start(&self, core: &Core, index: usize) -> Option<ReaderStart> {
        let target = self.chapter_at(index)?;
        let chapters: Vec<Chapter> = self.chapters_asc().into_iter().cloned().collect();
        let start = chapters.iter().position(|c| c.id == target.id).unwrap_or(0);
        let branches: HashMap<u64, Option<u64>> = chapters.iter().map(|c| (c.id, self.branch_of(c))).collect();
        let local: HashMap<u64, PathBuf> = chapters
            .iter()
            .filter_map(|c| {
                let row = self.downloads.get(&chapter_key(c.id, branches[&c.id]))?;
                let dir = PathBuf::from(&row.path);
                library::chapter_complete(&dir).then_some((c.id, dir))
            })
            .collect();
        let _ = core.db.upsert_title(&title_row(self.site, &self.data.details));
        Some(ReaderStart {
            site: self.site,
            slug: self.slug.clone(),
            title_id: self.title_id(),
            title: self.data.details.short.display_name().to_string(),
            chapters,
            branches,
            local,
            index: start,
            offline: self.data.offline,
        })
    }

    /// Задания загрузчика для глав. Закрытые главы пропускаются, их число
    /// возвращается вторым.
    pub fn chapter_jobs(&self, chapters: &[&Chapter]) -> (Vec<(String, Job)>, usize) {
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
        (jobs, locked)
    }

    /// Скачанные целиком главы выбранной ветки по томам, по порядку номеров.
    pub fn export_plan(&self) -> BTreeMap<String, Vec<ExportChapter>> {
        let mut by_volume: BTreeMap<String, Vec<(f32, ExportChapter)>> = BTreeMap::new();
        for chapter in self.chapters_asc() {
            let Some(row) = self.downloads.get(&self.chapter_key_of(chapter)) else {
                continue;
            };
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
        by_volume
            .into_iter()
            .map(|(volume, mut chapters)| {
                chapters.sort_by(|a, b| a.0.total_cmp(&b.0));
                (volume, chapters.into_iter().map(|(_, c)| c).collect())
            })
            .collect()
    }

    /// Глава по id перевода из закладки и ветка этого перевода.
    pub fn find_chapter(&self, item_id: u64) -> Option<(&Chapter, Option<u64>)> {
        self.data
            .chapters
            .iter()
            .find_map(|c| match c.branches.iter().find(|b| b.id == item_id) {
                Some(b) => Some((c, b.branch_id)),
                None => (c.id == item_id).then_some((c, None)),
            })
    }

    // --- Серии ---

    /// Серии в порядке показа.
    pub fn episodes_shown(&self) -> Vec<&Episode> {
        let mut list: Vec<&Episode> = self.data.episodes.iter().collect();
        if self.newest_first {
            list.reverse();
        }
        list
    }

    pub fn episode_at(&self, index: usize) -> Option<&Episode> {
        self.episodes_shown().get(index).copied()
    }

    pub fn episode_rows(&self) -> Vec<EpisodeRow<'_>> {
        self.episodes_shown()
            .into_iter()
            .map(|episode| {
                let key = episode_key(episode.id);
                EpisodeRow {
                    episode,
                    read: Read::of(self.progress.get(&key)),
                    download: self.downloads.get(&key),
                }
            })
            .collect()
    }

    /// Запоминает серию с плеерами. Команды для выбора берутся из первой
    /// загруженной серии: у какой есть собственный плеер.
    pub fn add_episode(&mut self, details: EpisodeDetails) {
        if self.teams.is_empty() {
            let mut teams: Vec<Team> = details
                .players
                .iter()
                .filter(|p| p.kind() == PlayerKind::Animelib)
                .filter_map(|p| {
                    Some(Team {
                        id: p.team.as_ref()?.id,
                        name: p.team_name().to_string(),
                        translation: p
                            .translation_type
                            .as_ref()
                            .map(|t| t.label.to_lowercase())
                            .unwrap_or_default(),
                    })
                })
                .collect();
            teams.dedup_by(|a, b| a.id == b.id);
            if self.team.is_none() {
                self.team = teams.first().map(|t| t.id);
            }
            self.teams = teams;
        }
        self.episodes.insert(details.id, details);
    }

    /// Плеер AnimeLib выбранной команды, иначе любой плеер AnimeLib.
    pub fn own_player<'a>(&self, details: &'a EpisodeDetails) -> Option<&'a Player> {
        let own: Vec<&Player> = details
            .players
            .iter()
            .filter(|p| p.kind() == PlayerKind::Animelib)
            .collect();
        own.iter()
            .find(|p| p.team.as_ref().map(|t| t.id) == self.team)
            .or(own.first())
            .copied()
    }

    /// Плеер Kodik выбранной команды, иначе любой.
    pub fn kodik_player<'a>(&self, details: &'a EpisodeDetails) -> Option<&'a Player> {
        details
            .players
            .iter()
            .find(|p| p.kind() == PlayerKind::Kodik && p.team.as_ref().map(|t| t.id) == self.team)
            .or_else(|| details.players.iter().find(|p| p.kind() == PlayerKind::Kodik))
    }

    /// С какой секунды продолжить серию. Досмотренную открываем с начала.
    pub fn resume_at(&self, episode_id: u64) -> Option<u64> {
        self.progress
            .get(&episode_key(episode_id))
            .filter(|p| !p.finished)
            .map(|p| p.position)
    }

    /// Отмечает серию начатой, не затирая уже записанный прогресс.
    pub fn mark_started(&mut self, core: &Core, episode_id: u64) {
        let key = episode_key(episode_id);
        if self.progress.contains_key(&key) {
            return;
        }
        self.save_progress(
            core,
            key,
            Progress {
                position: 0,
                total: 0,
                finished: false,
            },
        );
    }

    /// Слежение за плеером: куда писать позицию и что отправить на сайт.
    pub fn track(&self, core: &Core, episode: &Episode, player: Option<&Player>) -> Track {
        Track {
            db: core.db.clone(),
            sync: core.sync.clone(),
            site: self.site,
            slug: self.slug.clone(),
            key: episode_key(episode.id),
            anime_id: self.title_id(),
            episode_id: episode.id,
            item_number: episode.item_number,
            player_id: player.map(|p| p.id),
            choice: player.and_then(Player::choice),
        }
    }

    /// Плеер, которым скачана серия, если её скачивали.
    pub fn downloaded_player(&self, episode_id: u64) -> Option<(PathBuf, u64)> {
        let row = self.downloads.get(&episode_key(episode_id))?;
        if row.state != DownloadState::Done {
            return None;
        }
        let player_id = match serde_json::from_str::<Job>(&row.job) {
            Ok(Job::Episode { player_id, .. }) => player_id,
            _ => return None,
        };
        Some((PathBuf::from(&row.path), player_id))
    }

    // --- Прогресс и отметки ---

    fn save_progress(&mut self, core: &Core, key: String, p: Progress) {
        let _ = core.db.save_progress(self.site, &self.slug, &key, p);
        self.progress.insert(key, p);
    }

    /// Ставит или снимает отметку «прочитано» у строки списка. На сайт уходит
    /// только «прочитано»: снять отметку там нельзя. Если для отметки серии нужны
    /// плееры, а их ещё нет, возвращает id серии: её надо загрузить и позвать
    /// `push_view_mark` ещё раз.
    pub fn mark_read(&mut self, core: &Core, index: usize, finished: bool) -> Option<u64> {
        let key = if self.site.is_anime() {
            episode_key(self.episode_at(index)?.id)
        } else {
            self.chapter_key_of(self.chapter_at(index)?)
        };
        if !finished {
            let _ = core.db.clear_progress(self.site, &self.slug, &key);
            self.progress.remove(&key);
            return None;
        }
        self.save_progress(
            core,
            key,
            Progress {
                position: 0,
                total: 0,
                finished: true,
            },
        );
        self.push_view_mark(core, index)
    }

    /// Отправляет на сайт отметку о прочтении строки списка. Возвращает id серии,
    /// если для отметки не хватает её плееров.
    pub fn push_view_mark(&self, core: &Core, index: usize) -> Option<u64> {
        if self.site.is_anime() {
            let episode = self.episode_at(index)?;
            let Some(details) = self.episodes.get(&episode.id) else {
                return (!self.data.offline).then_some(episode.id);
            };
            let player = self
                .own_player(details)
                .or_else(|| {
                    details
                        .players
                        .iter()
                        .find(|p| p.team.as_ref().map(|t| t.id) == self.team)
                })
                .or_else(|| details.players.first())?;
            core.sync.push(Mark::EpisodeViewed {
                anime_id: self.title_id(),
                player_id: player.id,
            });
            return None;
        }
        let chapter = self.chapter_at(index)?;
        let chapter_id = chapter.branch(self.branch_of(chapter)).map_or(chapter.id, |b| b.id);
        core.sync.push(Mark::ChapterViewed {
            site: self.site,
            title_id: self.title_id(),
            chapter_id,
        });
        None
    }

    // --- Закладка ---

    /// Пришёл ответ о закладке при открытии страницы. Тайтл в закладках – значит,
    /// и в библиотеке на устройстве, чтобы открываться без сети. Флаг, поставленный
    /// без входа, не снимается.
    pub fn bookmark_loaded(&mut self, core: &Core, bookmark: Option<Bookmark>) {
        if bookmark.is_some() {
            self.set_local_library(core, true);
        }
        self.bookmark = bookmark;
        self.bookmark_known = true;
    }

    /// Закладку поменяли с этой страницы.
    pub fn bookmark_saved(&mut self, core: &Core, bookmark: Option<Bookmark>) {
        self.set_local_library(core, bookmark.is_some());
        self.bookmark = bookmark;
        self.bookmark_known = true;
    }

    /// Переключает флаг «в библиотеке» на устройстве. Возвращает новое значение.
    pub fn toggle_local_library(&mut self, core: &Core) -> anyhow::Result<bool> {
        core.db.upsert_title(&title_row(self.site, &self.data.details))?;
        let value = !self.in_library;
        core.db.set_in_library(self.site, &self.slug, value)?;
        self.in_library = value;
        Ok(value)
    }

    fn set_local_library(&mut self, core: &Core, value: bool) {
        let _ = core.db.upsert_title(&title_row(self.site, &self.data.details));
        if core.db.set_in_library(self.site, &self.slug, value).is_ok() {
            self.in_library = value;
        }
    }

    /// Куда ведёт «Продолжить» по закладке. Место из закладки уточняется своим:
    /// если главу здесь уже дочитали, открывается следующая, а начатая
    /// продолжается со своей страницы. Подпись кнопки и переход берутся отсюда же,
    /// чтобы кнопка не обещала одно, а открывала другое.
    pub fn resume(&self) -> Option<Resume> {
        let bookmark = self.bookmark.as_ref()?;
        let item_id = bookmark.item_id?;
        let progress = bookmark.progress.as_deref().unwrap_or_default();
        if self.site.is_anime() {
            let episode = self.data.episodes.iter().find(|e| e.id == item_id)?;
            let local = self.progress.get(&episode_key(episode.id));
            let secs = match local {
                Some(p) if !p.finished && p.position > 0 => Some(p.position),
                Some(_) => None,
                None => parse_timecode(progress).filter(|s| *s > 0),
            };
            return Some(Resume::Episode {
                episode_id: episode.id,
                secs,
                team: bookmark.meta.team,
            });
        }
        let (chapter, bookmark_branch) = self.find_chapter(item_id)?;
        // Ветку закладки берём, только если у тайтла выбор веток, как в списке глав.
        let branch = if self.branch.is_some() && bookmark_branch.is_some() {
            bookmark_branch
        } else {
            self.branch_of(chapter)
        };
        let resume = |chapter_id, page, from_bookmark| Resume::Chapter {
            chapter_id,
            branch,
            page,
            from_bookmark,
        };
        match self.progress.get(&chapter_key(chapter.id, branch)) {
            Some(p) if p.finished => {
                let list = self.chapters_asc();
                let at = list.iter().position(|c| c.id == chapter.id)?;
                let next = list[at + 1..].iter().find(|c| self.is_open(c))?;
                Some(resume(next.id, None, false))
            }
            Some(p) if p.position > 0 => Some(resume(chapter.id, Some(p.position + 1), false)),
            _ => {
                let page = progress.parse::<u64>().ok().filter(|p| *p > 1);
                Some(resume(chapter.id, page, true))
            }
        }
    }

    /// Готовит переход по «Продолжить»: переключает ветку и команду, а место из
    /// закладки записывает как своё, чтобы читалка или плеер начали с него.
    /// Возвращает индекс главы или серии в порядке показа.
    pub fn prepare_resume(&mut self, core: &Core, resume: &Resume) -> Option<usize> {
        match *resume {
            Resume::Chapter {
                chapter_id,
                branch,
                page,
                from_bookmark,
            } => {
                // Глава из другой ветки перевода: переключаемся на неё, как сайт.
                if branch.is_some() && self.branch.is_some() && self.branch != branch {
                    self.branch = branch;
                }
                let index = self.index_of_chapter(chapter_id)?;
                if from_bookmark && let Some(page) = page {
                    let key = self.chapter_key_of(self.chapter_at(index)?);
                    self.save_progress(
                        core,
                        key,
                        Progress {
                            position: page - 1,
                            total: 0,
                            finished: false,
                        },
                    );
                }
                Some(index)
            }
            Resume::Episode { episode_id, secs, team } => {
                if let Some(team) = team
                    && self.teams.iter().any(|t| t.id == team)
                {
                    self.team = Some(team);
                }
                let key = episode_key(episode_id);
                if let Some(secs) = secs
                    && !self.progress.contains_key(&key)
                {
                    self.save_progress(
                        core,
                        key,
                        Progress {
                            position: secs,
                            total: 0,
                            finished: false,
                        },
                    );
                }
                self.episodes_shown().iter().position(|e| e.id == episode_id)
            }
        }
    }
}

/// Кладёт тайтл в папку на сайте или, с `folder: None`, убирает из закладок.
pub async fn set_folder(
    core: &Core,
    site: Site,
    slug: &str,
    folder: Option<u64>,
    current: Option<u64>,
) -> libsocial::Result<Option<Bookmark>> {
    match (folder, current) {
        (Some(folder), _) => core.client.set_bookmark(site, slug, folder, None).await.map(Some),
        (None, Some(id)) => core.client.delete_bookmark(site, id).await.map(|()| None),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::db::DownloadRow;

    fn temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("polka-title-{name}-{}-{n}", std::process::id()))
    }

    /// Главы 1–4 в одной ветке, у каждой перевод с id `100 + номер`. Третья закрыта.
    fn manga() -> TitleData {
        let chapter = |n: u64, open: bool| {
            json!({
                "id": n, "volume": "1", "number": n.to_string(), "item_number": n,
                "branches": [{"id": 100 + n, "branch_id": null, "teams": [],
                              "restricted_view": {"is_open": open}}],
            })
        };
        TitleData {
            details: serde_json::from_value(json!({"id": 7, "name": "X", "slug_url": "7--x"})).unwrap(),
            chapters: serde_json::from_value(json!([
                chapter(1, true),
                chapter(2, true),
                chapter(3, false),
                chapter(4, true)
            ]))
            .unwrap(),
            branches: Vec::new(),
            episodes: Vec::new(),
            offline: false,
        }
    }

    fn page(core: &Core) -> TitlePage {
        TitlePage::new(core, Site::MangaLib, "7--x".into(), manga())
    }

    fn bookmark(item_id: u64, progress: &str) -> Bookmark {
        serde_json::from_value(json!({"id": 1, "status": 1, "item_id": item_id, "progress": progress})).unwrap()
    }

    fn finish(core: &Core, p: &mut TitlePage, chapter: u64) {
        p.save_progress(
            core,
            chapter_key(chapter, None),
            Progress {
                position: 0,
                total: 10,
                finished: true,
            },
        );
    }

    #[tokio::test]
    async fn continue_skips_read_and_locked() {
        let dir = temp_dir("continue");
        let core = Core::for_tests(&dir, "http://127.0.0.1:9/api");
        let mut p = page(&core);
        assert_eq!(p.continue_target(), Some(1));
        finish(&core, &mut p, 2);
        // После второй идёт закрытая третья: читать предлагаем четвёртую.
        assert_eq!(p.continue_target(), Some(4));
        let chapters = p.chapters();
        let (jobs, locked) = p.chapter_jobs(&chapters);
        assert_eq!((jobs.len(), locked), (3, 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resume_from_bookmark() {
        let dir = temp_dir("resume");
        let core = Core::for_tests(&dir, "http://127.0.0.1:9/api");
        let mut p = page(&core);
        assert_eq!(p.resume(), None);

        // Своего места нет: страница из закладки.
        p.bookmark = Some(bookmark(102, "5"));
        let r = p.resume().unwrap();
        assert_eq!(
            r,
            Resume::Chapter {
                chapter_id: 2,
                branch: None,
                page: Some(5),
                from_bookmark: true,
            }
        );
        // Переход записывает место из закладки как своё, и читалка начнёт с него.
        let index = p.prepare_resume(&core, &r).unwrap();
        assert_eq!(p.chapter_at(index).unwrap().id, 2);
        assert_eq!(
            core.db
                .progress(Site::MangaLib, "7--x", "c2-b0")
                .unwrap()
                .unwrap()
                .position,
            4
        );

        // Своё место важнее закладки.
        p.save_progress(
            &core,
            chapter_key(2, None),
            Progress {
                position: 7,
                total: 10,
                finished: false,
            },
        );
        assert!(matches!(
            p.resume(),
            Some(Resume::Chapter {
                page: Some(8),
                from_bookmark: false,
                ..
            })
        ));

        // Главу уже дочитали здесь: следующая открытая, через закрытую третью.
        finish(&core, &mut p, 2);
        assert!(matches!(
            p.resume(),
            Some(Resume::Chapter {
                chapter_id: 4,
                page: None,
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_mark_goes_to_site_with_branch_id() {
        let dir = temp_dir("mark");
        let core = Core::for_tests(&dir, "http://127.0.0.1:9/api");
        let mut p = page(&core);
        p.newest_first = false;
        assert_eq!(p.mark_read(&core, 1, true), None);
        let marks = core.db.pending_marks(10).unwrap();
        assert_eq!(marks.len(), 1);
        assert!(marks[0].1.contains("\"chapter_id\":102"), "{marks:?}");
        // Снятие отметки на сайт не уходит.
        assert_eq!(p.mark_read(&core, 1, false), None);
        assert_eq!(core.db.pending_marks(10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn anime_mark_needs_players() {
        let dir = temp_dir("anime");
        let core = Core::for_tests(&dir, "http://127.0.0.1:9/api");
        let data = TitleData {
            details: serde_json::from_value(json!({"id": 9, "name": "A", "slug_url": "9--a"})).unwrap(),
            chapters: Vec::new(),
            branches: Vec::new(),
            episodes: serde_json::from_value(json!([{"id": 41, "number": "1", "season": "1"}])).unwrap(),
            offline: false,
        };
        let mut p = TitlePage::new(&core, Site::AnimeLib, "9--a".into(), data);
        // Плееров серии ещё нет: страница должна их загрузить.
        assert_eq!(p.mark_read(&core, 0, true), Some(41));
        p.add_episode(
            serde_json::from_value(json!({"id": 41, "number": "1", "players": [
                {"id": 5, "player": "Animelib", "team": {"id": 3, "name": "Команда"},
                 "translation_type": {"id": 2, "label": "Озвучка"}}
            ]}))
            .unwrap(),
        );
        assert_eq!(p.team, Some(3));
        assert_eq!(p.push_view_mark(&core, 0), None);
        let marks = core.db.pending_marks(10).unwrap();
        assert!(marks.iter().any(|(_, m, _)| m.contains("\"player_id\":5")), "{marks:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn offline_title_from_downloads() {
        let dir = temp_dir("offline");
        let core = Core::for_tests(&dir, "http://127.0.0.1:9/api");
        let data = manga();
        core.db.upsert_title(&title_row(Site::MangaLib, &data.details)).unwrap();
        let job = Job::chapter(&data.chapters[1], None);
        let row = DownloadRow {
            site: Site::MangaLib,
            slug: "7--x".into(),
            item: job.item_key(),
            label: "Глава 2".into(),
            sort: job.sort(),
            state: DownloadState::Queued,
            done: 0,
            total: 0,
            error: None,
            path: "/nowhere".into(),
            job: serde_json::to_string(&job).unwrap(),
        };
        core.db.enqueue(&row).unwrap();
        // Недокачанное без сети не показываем.
        assert!(offline(&core, Site::MangaLib, "7--x").unwrap().chapters.is_empty());
        core.db
            .set_state(Site::MangaLib, "7--x", &row.item, DownloadState::Done, None)
            .unwrap();
        let t = offline(&core, Site::MangaLib, "7--x").unwrap();
        assert!(t.offline);
        assert_eq!(t.chapters.len(), 1);
        // Отметка о прочтении без сети уйдёт с id перевода, а не главы из списка.
        assert_eq!(t.chapters[0].branches[0].id, 102);
        let _ = std::fs::remove_dir_all(dir);
    }
}
