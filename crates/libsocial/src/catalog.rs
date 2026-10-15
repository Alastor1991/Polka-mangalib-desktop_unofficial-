//! Каталог манги, ранобэ и аниме: полный фильтр сайта, сортировки, случайный порядок.
//!
//! Каталог – это `GET /manga` (MangaLib, SlashLib, RanobeLib, HentaiLib) и `GET /anime`
//! (только AnimeLib). Вход не нужен. Что здесь неочевидно:
//!
//! - у аниме в `meta` нет `has_next_page`, поэтому следующую страницу определяют по
//!   `links.next`; так делает `listing::Page` и все списки этого крейта;
//! - без `fields[]=rate` и `fields[]=rate_avg` оценка в карточке нулевая;
//! - `site_id[]` сервер пересекает с тем, что разрешено для `Site-Id` запроса: из
//!   MangaLib нельзя увидеть SlashLib и HentaiLib, а RanobeLib виден как `site_id[]=3`;
//! - случайный порядок со второй страницы требует пару `seed` и `random_order` из
//!   `meta` предыдущей страницы;
//! - фильтры другого вида контента (`episodes_min` на `/manga`) сервер молча
//!   игнорирует, а неверные сортировки, поля и форматы отвергает с 422.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::listing::Page;
use crate::models::TitleShort;
use crate::site::Site;

/// Размер страницы каталога.
pub const CATALOG_PAGE_SIZE: u32 = 60;

/// Сколько оценок сайт требует вместе с сортировкой по рейтингу, чтобы наверх не
/// попали тайтлы с парой оценок.
pub const MIN_VOTES_FOR_RATING_SORT: u32 = 50;

/// Наибольший номер формата выпуска, который принимает `/manga`. Форматы 8–10
/// (веб-новелла, лайт-новелла, роман) есть в справочнике, но сервер отвечает на них 422.
pub const MAX_FILTERABLE_FORMAT: u64 = 7;

// ---------------------------------------------------------------------------------------
// Разборщики
// ---------------------------------------------------------------------------------------

/// Объект, а если сервер прислал `[]`, `null` или что-то другое, значение по умолчанию.
pub(crate) fn lenient_object<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + DeserializeOwned,
{
    let value = Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Список строк. Одиночная строка становится списком из одной, `null` – пустым списком.
pub(crate) fn string_list<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) if !s.is_empty() => vec![s],
        _ => Vec::new(),
    })
}

// ---------------------------------------------------------------------------------------
// Перечисления параметров
// ---------------------------------------------------------------------------------------

/// Сортировка каталога. Допустимые значения у манги и аниме разные: сервер на
/// чужую сортировку отвечает 422, поэтому `Client::catalog_filtered` проверяет её
/// заранее.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CatalogSort {
    /// По популярности (`rating_score`), порядок сервера по умолчанию.
    #[default]
    Popularity,
    /// По средней оценке (`rate_avg`). Вместе с ней нужен `rate_min=50`, см.
    /// `MIN_VOTES_FOR_RATING_SORT`: фильтр ставится сам.
    Rating,
    Views,
    /// По дате добавления на сайт.
    Added,
    /// По названию, A–Z.
    Name,
    /// По русскому названию, А–Я.
    RusName,
    ReleaseDate,
    /// Только манга: по числу глав.
    ChapterCount,
    /// Только манга: по дате последней главы.
    LastChapter,
    /// Только аниме: по числу серий.
    EpisodeCount,
    /// Только аниме: по дате последней серии.
    LastEpisode,
    /// Случайный порядок. Страницы после первой требуют `RandomCursor`.
    Random,
}

impl CatalogSort {
    /// Сортировки, которые принимает `/manga`.
    pub const MANGA: [CatalogSort; 10] = [
        CatalogSort::Popularity,
        CatalogSort::Rating,
        CatalogSort::Views,
        CatalogSort::Added,
        CatalogSort::Name,
        CatalogSort::RusName,
        CatalogSort::ReleaseDate,
        CatalogSort::ChapterCount,
        CatalogSort::LastChapter,
        CatalogSort::Random,
    ];

    /// Сортировки, которые принимает `/anime`.
    pub const ANIME: [CatalogSort; 10] = [
        CatalogSort::Popularity,
        CatalogSort::Rating,
        CatalogSort::Views,
        CatalogSort::Added,
        CatalogSort::Name,
        CatalogSort::RusName,
        CatalogSort::ReleaseDate,
        CatalogSort::EpisodeCount,
        CatalogSort::LastEpisode,
        CatalogSort::Random,
    ];

    /// Список для выпадающего меню сайта: у аниме и манги он разный.
    pub fn available(anime: bool) -> &'static [CatalogSort] {
        if anime { &Self::ANIME } else { &Self::MANGA }
    }

    /// Значение `sort_by`. Популярность сервер подразумевает сам, её не отправляют.
    pub fn param(self) -> Option<&'static str> {
        match self {
            CatalogSort::Popularity => None,
            CatalogSort::Rating => Some("rate_avg"),
            CatalogSort::Views => Some("views"),
            CatalogSort::Added => Some("created_at"),
            CatalogSort::Name => Some("name"),
            CatalogSort::RusName => Some("rus_name"),
            CatalogSort::ReleaseDate => Some("releaseDate"),
            CatalogSort::ChapterCount => Some("chap_count"),
            CatalogSort::LastChapter => Some("last_chapter_at"),
            CatalogSort::EpisodeCount => Some("episodes_count"),
            CatalogSort::LastEpisode => Some("last_episode_at"),
            CatalogSort::Random => Some("random_order"),
        }
    }

    pub fn supports(self, anime: bool) -> bool {
        Self::available(anime).contains(&self)
    }

    /// Подпись, как на сайте.
    pub fn label(self) -> &'static str {
        match self {
            CatalogSort::Popularity => "По популярности",
            CatalogSort::Rating => "По рейтингу",
            CatalogSort::Views => "По просмотрам",
            CatalogSort::Added => "По дате добавления",
            CatalogSort::Name => "По названию (A–Z)",
            CatalogSort::RusName => "По названию (А–Я)",
            CatalogSort::ReleaseDate => "По дате релиза",
            CatalogSort::ChapterCount => "По количеству глав",
            CatalogSort::LastChapter => "По дате обновления",
            CatalogSort::EpisodeCount => "По количеству эпизодов",
            CatalogSort::LastEpisode => "По дате обновления",
            CatalogSort::Random => "Случайно",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    #[default]
    Descending,
    Ascending,
}

impl SortOrder {
    pub fn param(self) -> &'static str {
        match self {
            SortOrder::Descending => "desc",
            SortOrder::Ascending => "asc",
        }
    }
}

/// Дополнительные поля карточки (`fields[]`). Без них сервер отдаёт нулевую оценку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CatalogField {
    /// Число оценок (`rating.votes`).
    Rate,
    /// Средняя оценка (`rating.average`).
    RateAvg,
    /// Папка закладки вошедшего пользователя.
    UserBookmark,
    ReleaseDate,
    /// Только манга.
    MangaStatusId,
    /// Только аниме.
    AnimeStatusId,
    /// Только манга: даты синхронизаций с Shikimori и AniList.
    Metadata,
    Moderated,
    /// Только манга.
    ExtraReleaseStatus,
}

impl CatalogField {
    /// Набор, который просит сам сайт для списка.
    pub const LIST: [CatalogField; 3] = [CatalogField::Rate, CatalogField::RateAvg, CatalogField::UserBookmark];

    pub fn param(self) -> &'static str {
        match self {
            CatalogField::Rate => "rate",
            CatalogField::RateAvg => "rate_avg",
            CatalogField::UserBookmark => "userBookmark",
            CatalogField::ReleaseDate => "releaseDate",
            CatalogField::MangaStatusId => "manga_status_id",
            CatalogField::AnimeStatusId => "anime_status_id",
            CatalogField::Metadata => "metadata",
            CatalogField::Moderated => "moderated",
            CatalogField::ExtraReleaseStatus => "extra_release_status",
        }
    }

    pub fn supports(self, anime: bool) -> bool {
        match self {
            CatalogField::MangaStatusId | CatalogField::Metadata | CatalogField::ExtraReleaseStatus => !anime,
            CatalogField::AnimeStatusId => anime,
            _ => true,
        }
    }
}

/// «Дополнительные главы» (`extra_release_status[]`). Только манга.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExtraRelease {
    Announced,
    Ongoing,
}

impl ExtraRelease {
    pub fn param(self) -> &'static str {
        match self {
            ExtraRelease::Announced => "announced",
            ExtraRelease::Ongoing => "ongoing",
        }
    }
}

/// Чьи тайтлы показывать: страницы человека, персонажа, команды и других сущностей
/// сайт строит тем же каталогом с `target_model` и `target_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TargetModel {
    People,
    Team,
    Character,
    Franchise,
    Publisher,
    User,
}

impl TargetModel {
    pub fn param(self) -> &'static str {
        match self {
            TargetModel::People => "people",
            TargetModel::Team => "team",
            TargetModel::Character => "character",
            TargetModel::Franchise => "franchise",
            TargetModel::Publisher => "publisher",
            TargetModel::User => "user",
        }
    }
}

/// Сущность, по тайтлам которой строится каталог. Одиночный `target_id` без модели
/// сервер игнорирует, поэтому они задаются вместе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub model: TargetModel,
    pub id: u64,
}

/// Пара из `meta` предыдущей страницы, без которой случайный порядок не листается.
///
/// Сервер на вторую страницу без `random_order` отвечает 422, а выдуманное число
/// отвергает как неверный формат, поэтому курсор берут только из ответа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RandomCursor {
    /// Строка из 32 шестнадцатеричных символов.
    pub seed: String,
    pub random_order: String,
}

impl RandomCursor {
    /// Курсор из `meta` страницы каталога, если сервер его прислал. `random_order`
    /// приходит числом на первой странице и строкой на следующих.
    pub fn of<T>(page: &Page<T>) -> Option<RandomCursor> {
        let text = |key: &str| match page.meta.get(key)? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Some(RandomCursor {
            seed: text("seed")?,
            random_order: text("random_order")?,
        })
    }
}

/// Диапазон значений с необязательными границами.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bounds<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

impl<T> Default for Bounds<T> {
    fn default() -> Self {
        Bounds { min: None, max: None }
    }
}

impl<T> Bounds<T> {
    pub fn at_least(min: T) -> Bounds<T> {
        Bounds {
            min: Some(min),
            max: None,
        }
    }

    pub fn between(min: T, max: T) -> Bounds<T> {
        Bounds {
            min: Some(min),
            max: Some(max),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }
}

// ---------------------------------------------------------------------------------------
// Фильтр
// ---------------------------------------------------------------------------------------

/// Все параметры формы каталога сайта. Пустое значение не отправляется: запрос по
/// умолчанию равен открытию каталога без фильтров.
///
/// Номера жанров, тегов, типов и прочего берутся из `Client::catalog_constants`,
/// папок закладок – из `Client::bookmark_folders`.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogFilter {
    /// Поиск по названию: русскому, английскому, оригинальному и альтернативным.
    pub text: String,
    /// Номера сайтов (`site_id[]`). Пусто – тот сайт, от имени которого идёт запрос.
    /// Если среди них есть AnimeLib (5), запрос уходит в `/anime`.
    pub site_ids: Vec<u8>,
    pub sort: CatalogSort,
    pub order: SortOrder,
    pub genres: Vec<u64>,
    pub genres_exclude: Vec<u64>,
    /// «Хотя бы один из выбранных» вместо «все выбранные».
    pub genres_any: bool,
    pub tags: Vec<u64>,
    pub tags_exclude: Vec<u64>,
    pub tags_any: bool,
    pub types: Vec<u64>,
    /// Статус тайтла (`status[]`).
    pub statuses: Vec<u64>,
    /// Статус перевода (`scanlate_status[]`). В форме сайта скрыт для аниме.
    pub scanlate_statuses: Vec<u64>,
    /// Возрастной рейтинг (`caution[]`). Для аниме номера из `ageRestrictionAnime`.
    pub age_ratings: Vec<u64>,
    /// Формат выпуска, 1–7. Для RanobeLib через API не работает.
    pub formats: Vec<u64>,
    pub formats_exclude: Vec<u64>,
    pub extra_release: Vec<ExtraRelease>,
    pub years: Bounds<u32>,
    /// Число глав. Для аниме не действует.
    pub chapters: Bounds<u32>,
    /// Число серий. Для манги не действует.
    pub episodes: Bounds<u32>,
    /// Средняя оценка, 0–10.
    pub rating: Bounds<f32>,
    /// Число оценок.
    pub votes: Bounds<u32>,
    /// `Some(true)` – только лицензированные, `Some(false)` – без них.
    pub licensed: Option<bool>,
    /// «Можно приобрести». Сайт вместе с ним ставит `licensed=1`, и здесь тоже.
    pub buy: bool,
    /// Только манга: нет перевода уже два месяца.
    pub long_no_translation: bool,
    /// Только манга: есть озвучка.
    pub has_voice: bool,
    pub has_droppable_cards: bool,
    /// «Команды в игноре» (нужен вход). Смысл значений выведен из интерфейса и
    /// вживую не проверен.
    pub ignored_teams: Option<bool>,
    /// Папки закладок пользователя: тайтл лежит в любой из них. Нужен вход.
    pub bookmarks: Vec<u64>,
    /// Тайтл не лежит ни в одной из этих папок. Нужен вход.
    pub bookmarks_exclude: Vec<u64>,
    pub target: Option<Target>,
    /// Только для модераторов: 0 на модерации, 1 принято, 2 отклонено, 3 на удалении.
    pub moderated: Vec<u8>,
    /// Курсор случайного порядка, нужен со второй страницы при `CatalogSort::Random`.
    pub random: Option<RandomCursor>,
    /// Только тайтлы, где уже есть глава или серия.
    pub only_with_content: bool,
    /// Дополнительные поля карточки. По умолчанию те, что просит сам сайт.
    pub fields: Vec<CatalogField>,
}

impl Default for CatalogFilter {
    fn default() -> Self {
        CatalogFilter {
            text: String::new(),
            site_ids: Vec::new(),
            sort: CatalogSort::default(),
            order: SortOrder::default(),
            genres: Vec::new(),
            genres_exclude: Vec::new(),
            genres_any: false,
            tags: Vec::new(),
            tags_exclude: Vec::new(),
            tags_any: false,
            types: Vec::new(),
            statuses: Vec::new(),
            scanlate_statuses: Vec::new(),
            age_ratings: Vec::new(),
            formats: Vec::new(),
            formats_exclude: Vec::new(),
            extra_release: Vec::new(),
            years: Bounds::default(),
            chapters: Bounds::default(),
            episodes: Bounds::default(),
            rating: Bounds::default(),
            votes: Bounds::default(),
            licensed: None,
            buy: false,
            long_no_translation: false,
            has_voice: false,
            has_droppable_cards: false,
            ignored_teams: None,
            bookmarks: Vec::new(),
            bookmarks_exclude: Vec::new(),
            target: None,
            moderated: Vec::new(),
            random: None,
            only_with_content: false,
            fields: CatalogField::LIST.to_vec(),
        }
    }
}

impl CatalogFilter {
    /// Поиск по названию без остальных фильтров.
    pub fn search(text: impl Into<String>) -> CatalogFilter {
        CatalogFilter {
            text: text.into(),
            ..Default::default()
        }
    }

    /// Тайтлы человека, персонажа, команды, франшизы, издателя или пользователя.
    pub fn of(model: TargetModel, id: u64) -> CatalogFilter {
        CatalogFilter {
            target: Some(Target { model, id }),
            ..Default::default()
        }
    }

    /// Уйдёт ли запрос в `/anime`.
    pub fn is_anime(&self, site: Site) -> bool {
        site.is_anime() || self.site_ids.contains(&Site::AnimeLib.id())
    }

    /// Параметры запроса. Ошибка, если сервер отверг бы сочетание с 422: чужая
    /// сортировка или поле, формат не из 1–7, вторая страница случайного порядка
    /// без курсора.
    pub fn to_query(&self, site: Site, page: u32) -> Result<Vec<(String, String)>> {
        let anime = self.is_anime(site);
        let kind = if anime { "аниме" } else { "манги" };
        if !self.sort.supports(anime) {
            return Err(Error::Other(format!(
                "сортировка «{}» недоступна для {kind}",
                self.sort.label()
            )));
        }
        if let Some(field) = self.fields.iter().find(|f| !f.supports(anime)) {
            return Err(Error::Other(format!(
                "поле карточки {} недоступно для {kind}",
                field.param()
            )));
        }
        if let Some(format) = self
            .formats
            .iter()
            .chain(&self.formats_exclude)
            .find(|f| **f == 0 || **f > MAX_FILTERABLE_FORMAT)
        {
            return Err(Error::Other(format!(
                "формат выпуска {format} сервер не принимает, допустимы 1–{MAX_FILTERABLE_FORMAT}"
            )));
        }
        let page = page.max(1);
        let random = self.sort == CatalogSort::Random;
        if random && page > 1 && self.random.is_none() {
            return Err(Error::Other(
                "для второй страницы случайного порядка нужен курсор из ответа первой".into(),
            ));
        }

        let mut p = Query::default();

        p.add("page", page.to_string());
        if !self.text.trim().is_empty() {
            p.add("q", self.text.trim().to_string());
        }
        for id in &self.site_ids {
            p.add("site_id[]", id.to_string());
        }
        if let Some(sort) = self.sort.param() {
            p.add("sort_by", sort.to_string());
        }
        if self.order != SortOrder::default() {
            p.add("sort_type", self.order.param().to_string());
        }
        if random && let Some(cursor) = &self.random {
            p.add("seed", cursor.seed.clone());
            p.add("random_order", cursor.random_order.clone());
        }
        for field in &self.fields {
            p.add("fields[]", field.param().to_string());
        }

        let lists: [(&str, &Vec<u64>); 11] = [
            ("genres[]", &self.genres),
            ("genres_exclude[]", &self.genres_exclude),
            ("tags[]", &self.tags),
            ("tags_exclude[]", &self.tags_exclude),
            ("types[]", &self.types),
            ("status[]", &self.statuses),
            ("scanlate_status[]", &self.scanlate_statuses),
            ("caution[]", &self.age_ratings),
            ("format[]", &self.formats),
            ("format_exclude[]", &self.formats_exclude),
            ("bookmarks[]", &self.bookmarks),
        ];
        for (key, ids) in lists {
            for id in ids {
                p.add(key, id.to_string());
            }
        }
        for id in &self.bookmarks_exclude {
            p.add("bookmarks_exclude[]", id.to_string());
        }
        if self.genres_any {
            p.add("genres_soft_search", flag(true));
        }
        if self.tags_any {
            p.add("tags_soft_search", flag(true));
        }
        for extra in &self.extra_release {
            p.add("extra_release_status[]", extra.param().to_string());
        }

        p.bounds(
            "year_min",
            "year_max",
            self.years.min.map(|v| v.to_string()),
            self.years.max.map(|v| v.to_string()),
        );
        // «Только с содержимым» ставит нижнюю границу единицей, если её не задали.
        let (mut chap_min, mut ep_min) = (self.chapters.min, self.episodes.min);
        if self.only_with_content {
            if anime {
                ep_min = ep_min.or(Some(1));
            } else {
                chap_min = chap_min.or(Some(1));
            }
        }
        p.bounds(
            "chap_count_min",
            "chap_count_max",
            chap_min.map(|v| v.to_string()),
            self.chapters.max.map(|v| v.to_string()),
        );
        p.bounds(
            "episodes_min",
            "episodes_max",
            ep_min.map(|v| v.to_string()),
            self.episodes.max.map(|v| v.to_string()),
        );
        p.bounds(
            "rating_min",
            "rating_max",
            self.rating.min.map(|v| v.to_string()),
            self.rating.max.map(|v| v.to_string()),
        );
        let mut votes_min = self.votes.min;
        if self.sort == CatalogSort::Rating {
            votes_min = votes_min.or(Some(MIN_VOTES_FOR_RATING_SORT));
        }
        p.bounds(
            "rate_min",
            "rate_max",
            votes_min.map(|v| v.to_string()),
            self.votes.max.map(|v| v.to_string()),
        );

        // Покупка предполагает лицензию, сайт ставит её сам.
        let licensed = self.licensed.or(self.buy.then_some(true));
        if let Some(on) = licensed {
            p.add("licensed", flag(on));
        }
        if self.buy {
            p.add("buy", flag(true));
        }
        for (key, on) in [
            ("long_no_translation", self.long_no_translation),
            ("has_voice", self.has_voice),
            ("has_droppable_cards", self.has_droppable_cards),
        ] {
            if on {
                p.add(key, flag(true));
            }
        }
        if let Some(on) = self.ignored_teams {
            p.add("ignored_teams", flag(on));
        }
        for level in &self.moderated {
            p.add("moderated[]", level.to_string());
        }
        if let Some(target) = self.target {
            p.add("target_model", target.model.param().to_string());
            p.add("target_id", target.id.to_string());
        }
        Ok(p.0)
    }
}

/// Сборщик параметров запроса.
#[derive(Default)]
struct Query(Vec<(String, String)>);

impl Query {
    fn add(&mut self, key: &str, value: impl ToString) {
        self.0.push((key.to_string(), value.to_string()));
    }

    fn bounds(&mut self, min_key: &str, max_key: &str, min: Option<String>, max: Option<String>) {
        if let Some(v) = min {
            self.add(min_key, v);
        }
        if let Some(v) = max {
            self.add(max_key, v);
        }
    }
}

fn flag(on: bool) -> &'static str {
    if on { "1" } else { "0" }
}

// ---------------------------------------------------------------------------------------
// Ответ и запрос
// ---------------------------------------------------------------------------------------

impl Client {
    /// Каталог с полным набором фильтров сайта. `site` – сайт, от имени которого
    /// идёт запрос; он же задаёт `/manga` или `/anime`.
    ///
    /// Оценки и число оценок заполнены, пока в `filter.fields` есть `Rate` и `RateAvg`.
    /// Курсор для следующей страницы случайного порядка даёт `RandomCursor::of`.
    pub async fn catalog_filtered(&self, site: Site, filter: &CatalogFilter, page: u32) -> Result<Page<TitleShort>> {
        let query = filter.to_query(site, page)?;
        let path = if filter.is_anime(site) { "anime" } else { "manga" };
        self.get_page(site, path, &query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(query: &[(String, String)], key: &str, value: &str) -> bool {
        query.iter().any(|(k, v)| k == key && v == value)
    }

    fn lacks(query: &[(String, String)], key: &str) -> bool {
        query.iter().all(|(k, _)| k != key)
    }

    #[test]
    fn default_filter_sends_only_page_and_fields() {
        let q = CatalogFilter::default().to_query(Site::MangaLib, 0).unwrap();
        assert!(has(&q, "page", "1"));
        assert!(has(&q, "fields[]", "rate"));
        assert!(has(&q, "fields[]", "rate_avg"));
        for key in ["sort_by", "sort_type", "site_id[]", "q", "licensed", "target_id"] {
            assert!(lacks(&q, key), "{key}");
        }
    }

    #[test]
    fn lists_flags_and_bounds() {
        let filter = CatalogFilter {
            text: "  берсерк ".into(),
            site_ids: vec![1, Site::RanobeLib.id()],
            sort: CatalogSort::Views,
            order: SortOrder::Ascending,
            genres: vec![34, 47],
            genres_exclude: vec![5],
            genres_any: true,
            tags: vec![9],
            tags_exclude: vec![10],
            tags_any: true,
            types: vec![1],
            statuses: vec![2],
            scanlate_statuses: vec![3],
            age_ratings: vec![4],
            formats: vec![7],
            formats_exclude: vec![2],
            extra_release: vec![ExtraRelease::Ongoing],
            years: Bounds::between(1990, 2000),
            chapters: Bounds::at_least(10),
            rating: Bounds::between(7.5, 10.0),
            votes: Bounds::at_least(500),
            licensed: Some(false),
            long_no_translation: true,
            has_voice: true,
            has_droppable_cards: true,
            ignored_teams: Some(true),
            bookmarks: vec![1, 2],
            bookmarks_exclude: vec![3],
            moderated: vec![0],
            target: Some(Target {
                model: TargetModel::People,
                id: 113727,
            }),
            ..Default::default()
        };
        let q = filter.to_query(Site::MangaLib, 3).unwrap();
        for (k, v) in [
            ("page", "3"),
            ("q", "берсерк"),
            ("site_id[]", "1"),
            ("site_id[]", "3"),
            ("sort_by", "views"),
            ("sort_type", "asc"),
            ("genres[]", "34"),
            ("genres[]", "47"),
            ("genres_exclude[]", "5"),
            ("genres_soft_search", "1"),
            ("tags[]", "9"),
            ("tags_exclude[]", "10"),
            ("tags_soft_search", "1"),
            ("types[]", "1"),
            ("status[]", "2"),
            ("scanlate_status[]", "3"),
            ("caution[]", "4"),
            ("format[]", "7"),
            ("format_exclude[]", "2"),
            ("extra_release_status[]", "ongoing"),
            ("year_min", "1990"),
            ("year_max", "2000"),
            ("chap_count_min", "10"),
            ("rating_min", "7.5"),
            ("rating_max", "10"),
            ("rate_min", "500"),
            ("licensed", "0"),
            ("long_no_translation", "1"),
            ("has_voice", "1"),
            ("has_droppable_cards", "1"),
            ("ignored_teams", "1"),
            ("bookmarks[]", "1"),
            ("bookmarks[]", "2"),
            ("bookmarks_exclude[]", "3"),
            ("moderated[]", "0"),
            ("target_model", "people"),
            ("target_id", "113727"),
        ] {
            assert!(has(&q, k, v), "{k}={v}: {q:?}");
        }
        for key in ["chap_count_max", "episodes_min", "buy", "rate_max"] {
            assert!(lacks(&q, key), "{key}");
        }
    }

    #[test]
    fn rating_sort_adds_vote_floor_unless_set() {
        let mut filter = CatalogFilter {
            sort: CatalogSort::Rating,
            ..Default::default()
        };
        let q = filter.to_query(Site::MangaLib, 1).unwrap();
        assert!(has(&q, "sort_by", "rate_avg"));
        assert!(has(&q, "rate_min", "50"));
        filter.votes = Bounds::at_least(1000);
        assert!(has(&filter.to_query(Site::MangaLib, 1).unwrap(), "rate_min", "1000"));
    }

    #[test]
    fn buy_implies_licensed() {
        let filter = CatalogFilter {
            buy: true,
            ..Default::default()
        };
        let q = filter.to_query(Site::MangaLib, 1).unwrap();
        assert!(has(&q, "buy", "1"));
        assert!(has(&q, "licensed", "1"));
    }

    #[test]
    fn content_floor_depends_on_kind() {
        let filter = CatalogFilter {
            only_with_content: true,
            ..Default::default()
        };
        let manga = filter.to_query(Site::MangaLib, 1).unwrap();
        assert!(has(&manga, "chap_count_min", "1"));
        assert!(lacks(&manga, "episodes_min"));
        let anime = filter.to_query(Site::AnimeLib, 1).unwrap();
        assert!(has(&anime, "episodes_min", "1"));
        assert!(lacks(&anime, "chap_count_min"));
    }

    #[test]
    fn sorts_are_checked_per_kind() {
        let chapters = CatalogFilter {
            sort: CatalogSort::ChapterCount,
            ..Default::default()
        };
        assert!(chapters.to_query(Site::MangaLib, 1).is_ok());
        assert!(chapters.to_query(Site::AnimeLib, 1).is_err());
        let episodes = CatalogFilter {
            sort: CatalogSort::LastEpisode,
            ..Default::default()
        };
        assert!(episodes.to_query(Site::MangaLib, 1).is_err());
        assert!(episodes.to_query(Site::AnimeLib, 1).is_ok());
        assert!(CatalogSort::MANGA.iter().all(|s| s.supports(false)));
        assert!(CatalogSort::ANIME.iter().all(|s| s.supports(true)));
    }

    #[test]
    fn anime_is_chosen_by_site_id_too() {
        let filter = CatalogFilter {
            site_ids: vec![5],
            sort: CatalogSort::EpisodeCount,
            ..Default::default()
        };
        assert!(filter.is_anime(Site::MangaLib));
        assert!(filter.to_query(Site::MangaLib, 1).is_ok());
    }

    #[test]
    fn fields_are_checked_per_kind() {
        let filter = CatalogFilter {
            fields: vec![CatalogField::Metadata],
            ..Default::default()
        };
        assert!(filter.to_query(Site::MangaLib, 1).is_ok());
        assert!(filter.to_query(Site::AnimeLib, 1).is_err());
    }

    #[test]
    fn unsupported_formats_are_rejected() {
        for formats in [vec![8], vec![0]] {
            let filter = CatalogFilter {
                formats,
                ..Default::default()
            };
            assert!(filter.to_query(Site::MangaLib, 1).is_err());
        }
    }

    #[test]
    fn random_needs_cursor_after_first_page() {
        let mut filter = CatalogFilter {
            sort: CatalogSort::Random,
            ..Default::default()
        };
        let first = filter.to_query(Site::MangaLib, 1).unwrap();
        assert!(has(&first, "sort_by", "random_order"));
        assert!(lacks(&first, "seed"));
        assert!(filter.to_query(Site::MangaLib, 2).is_err());
        filter.random = Some(RandomCursor {
            seed: "6f722d27ae79acdce142486711099825".into(),
            random_order: "0.617244741235508".into(),
        });
        let second = filter.to_query(Site::MangaLib, 2).unwrap();
        assert!(has(&second, "seed", "6f722d27ae79acdce142486711099825"));
        assert!(has(&second, "random_order", "0.617244741235508"));
        // Курсор имеет смысл только при случайном порядке.
        filter.sort = CatalogSort::Name;
        assert!(lacks(&filter.to_query(Site::MangaLib, 2).unwrap(), "seed"));
    }

    #[test]
    fn random_cursor_accepts_number_and_string() {
        let first: Page<u32> = serde_json::from_value(serde_json::json!({
            "data": [],
            "meta": {"seed": "abc", "random_order": 0.617244741235508}
        }))
        .unwrap();
        assert_eq!(
            RandomCursor::of(&first),
            Some(RandomCursor {
                seed: "abc".into(),
                random_order: "0.617244741235508".into()
            })
        );
        let plain: Page<u32> =
            serde_json::from_value(serde_json::json!({"data": [], "meta": {"seed": "abc"}})).unwrap();
        assert_eq!(RandomCursor::of(&plain), None);
    }
}
