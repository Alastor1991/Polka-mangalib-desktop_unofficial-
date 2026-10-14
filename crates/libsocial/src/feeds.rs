//! Ленты главной страницы, последние обновления, топ просмотров и новости.
//!
//! Вход нужен только для `user_latest_updates`. Ответы зависят от `Site-Id`.
//! Что здесь неочевидно:
//!
//! - в карточках лент оценка нулевая (`rating.average = "0"`), показывать её нельзя;
//!   оценки есть только у `top_views_page` и в каталоге с `fields[]=rate`, `rate_avg`;
//! - у аниме в `meta` нет `has_next_page`, следующую страницу выдаёт `links.next`;
//! - в последних обновлениях лежит `metadata.latest_items` (несколько глав или серий),
//!   в «обновлениях популярных» на главной `metadata.last_item` (одна);
//! - у глав в лентах `restricted_view.is_open` бывает `false` и у обычных: закрытой
//!   главу считают, только если есть цена или срок раннего доступа;
//! - «Сейчас читают» без `popularity` приходит группами по три тайтла, с `popularity`
//!   плоским списком по 56; группы манги 1–3, аниме 21–23;
//! - `hot-newest` на проверенных сайтах пуст, сайт показывает его избранным;
//! - картинки новостей и баннеров лежат на хосте API (`/uploads/...`), не на CDN обложек;
//!   в новости они подставлены в документ, в баннерах остаются путём (`Client::upload_url`);
//! - картинка в тексте новости ссылается на вложение по имени, не по адресу.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{Page, RawList, lenient_object};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::{Cover, Label, RestrictedView, Team, TitleShort};
use crate::rich::Doc;
use crate::search::UserCard;
use crate::serde_util::{lenient_vec, null_default, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;

// ---------------------------------------------------------------------------------------
// Карточки лент
// ---------------------------------------------------------------------------------------

/// Проигрыватель серии в ленте: озвучка или субтитры от команды.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedPlayer {
    /// `Animelib`, `Kodik`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub player: String,
    #[serde(default, deserialize_with = "null_default")]
    pub translation_type: Option<Label>,
    #[serde(default, deserialize_with = "null_default")]
    pub team: Option<Team>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
}

/// Глава или серия в ленте обновлений.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedItem {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    /// `chapter` или `episodes`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub model: String,
    /// Том. У серий нет.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub volume: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub number_secondary: Option<String>,
    /// Сезон серии.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub season: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub branch_id: Option<u64>,
    /// Порядковый номер серии в тайтле.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub item_number: Option<u64>,
    #[serde(default)]
    pub restricted_view: Option<RestrictedView>,
    /// Только у серий.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub players: Vec<FeedPlayer>,
}

impl FeedItem {
    pub fn is_episode(&self) -> bool {
        self.model == "episodes"
    }

    /// Платная глава или ранний доступ. Один `is_open = false` об этом не говорит.
    pub fn is_restricted(&self) -> bool {
        self.restricted_view
            .as_ref()
            .is_some_and(|r| !r.is_open && (r.expired_at.is_some() || r.price.as_ref().is_some_and(|p| !p.is_null())))
    }
}

/// Сколько глав или серий у тайтла в ленте обновлений. Из двух чисел заполнено одно:
/// у манги `uploaded`, у аниме `total`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemsCount {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub uploaded: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub total: Option<u64>,
}

/// Тайтл в ленте: карточка каталога плюс последние главы или серии.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawFeedTitle")]
pub struct FeedTitle {
    pub title: TitleShort,
    pub last_item_at: Option<String>,
    /// Новые главы или серии, свежие первыми. На главной в «обновлениях популярных»
    /// одна, в `latest_updates` до нескольких.
    pub items: Vec<FeedItem>,
    pub items_count: Option<ItemsCount>,
    /// Просмотры за выбранный период. Есть только у `top_views_page`.
    pub views: Option<u64>,
}

#[derive(Deserialize)]
struct RawFeedTitle {
    #[serde(flatten)]
    title: TitleShort,
    #[serde(default)]
    last_item_at: Option<String>,
    #[serde(default, deserialize_with = "lenient_object")]
    metadata: RawFeedMeta,
    #[serde(default)]
    items_count: Option<ItemsCount>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    views_day: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    views_week: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    views_month: Option<u64>,
}

#[derive(Default, Deserialize)]
struct RawFeedMeta {
    #[serde(default)]
    last_item: Option<FeedItem>,
    #[serde(default, deserialize_with = "lenient_object")]
    latest_items: RawLatestItems,
}

#[derive(Default, Deserialize)]
struct RawLatestItems {
    #[serde(default, deserialize_with = "lenient_vec")]
    items: Vec<FeedItem>,
}

impl From<RawFeedTitle> for FeedTitle {
    fn from(raw: RawFeedTitle) -> FeedTitle {
        let mut items = raw.metadata.latest_items.items;
        if items.is_empty() {
            items.extend(raw.metadata.last_item);
        }
        FeedTitle {
            title: raw.title,
            last_item_at: raw.last_item_at,
            items,
            items_count: raw.items_count,
            views: raw.views_day.or(raw.views_week).or(raw.views_month),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Топ просмотров
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TopTime {
    Day,
    Week,
    Month,
}

impl TopTime {
    pub fn param(self) -> &'static str {
        match self {
            TopTime::Day => "day",
            TopTime::Week => "week",
            TopTime::Month => "month",
        }
    }
}

/// Группа «Сейчас читают» или «Сейчас смотрят» (`popularity`). У манги и ранобэ
/// три группы, у аниме три других.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TopGroup {
    /// Манга и ранобэ: новинки.
    New,
    /// Манга и ранобэ: набирающие популярность.
    Rising,
    /// Манга и ранобэ: популярные.
    Popular,
    /// Аниме: завершённое.
    Completed,
    /// Аниме: онгоинг.
    Ongoing,
    /// Аниме: полнометражное.
    Features,
}

impl TopGroup {
    pub const MANGA: [TopGroup; 3] = [TopGroup::New, TopGroup::Rising, TopGroup::Popular];
    pub const ANIME: [TopGroup; 3] = [TopGroup::Completed, TopGroup::Ongoing, TopGroup::Features];

    pub fn id(self) -> u32 {
        match self {
            TopGroup::New => 1,
            TopGroup::Rising => 2,
            TopGroup::Popular => 3,
            TopGroup::Completed => 21,
            TopGroup::Ongoing => 22,
            TopGroup::Features => 23,
        }
    }

    pub fn from_id(id: u32) -> Option<TopGroup> {
        Self::MANGA.into_iter().chain(Self::ANIME).find(|g| g.id() == id)
    }

    pub fn is_anime(self) -> bool {
        self.id() > 20
    }

    pub fn for_site(site: Site) -> &'static [TopGroup] {
        if site.is_anime() { &Self::ANIME } else { &Self::MANGA }
    }

    pub fn label(self) -> &'static str {
        match self {
            TopGroup::New => "Новинки",
            TopGroup::Rising => "Набирающие популярность",
            TopGroup::Popular => "Популярные",
            TopGroup::Completed => "Завершённое",
            TopGroup::Ongoing => "Онгоинг",
            TopGroup::Features => "Полнометражное",
        }
    }
}

/// Тайтл в группе «Сейчас читают». Карточка без оценки.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopEntry {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(rename = "media")]
    pub title: TitleShort,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopViewsGroup {
    /// Номер группы, см. `TopGroup::from_id`.
    pub popularity: u32,
    pub entries: Vec<TopEntry>,
}

/// «Сейчас читают» группами: три тайтла в каждой.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RawTopViews")]
pub struct TopViews {
    /// `day`, `week` или `month`.
    pub time: String,
    pub groups: Vec<TopViewsGroup>,
}

#[derive(Default, Deserialize)]
struct RawTopViews {
    #[serde(default, deserialize_with = "string_lenient")]
    time_filter: String,
    /// Пустой набор сервер присылает как `[]`.
    #[serde(default, deserialize_with = "lenient_object")]
    items: BTreeMap<String, Value>,
}

impl From<RawTopViews> for TopViews {
    fn from(raw: RawTopViews) -> TopViews {
        let mut groups: Vec<TopViewsGroup> = raw
            .items
            .into_iter()
            .filter_map(|(key, value)| {
                let entries: Vec<TopEntry> = value
                    .as_array()?
                    .iter()
                    .filter_map(|e| serde_json::from_value(e.clone()).ok())
                    .collect();
                Some(TopViewsGroup {
                    popularity: key.parse().ok()?,
                    entries,
                })
            })
            .collect();
        groups.sort_by_key(|g| g.popularity);
        TopViews {
            time: raw.time_filter,
            groups,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Блоки главной
// ---------------------------------------------------------------------------------------

/// Рекламный баннер. Пути картинок относительные, адрес собирает `Client::upload_url`.
/// Премиум-пользователям сайт баннеры не показывает.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawBanner")]
pub struct Banner {
    pub id: u64,
    /// Куда ведёт баннер. Это рекламная ссылка.
    pub url: String,
    pub desktop: Option<String>,
    pub mobile: Option<String>,
    pub site_ids: Vec<u8>,
}

#[derive(Deserialize)]
struct RawBanner {
    #[serde(default, deserialize_with = "u64_lenient")]
    id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    url: String,
    #[serde(default, deserialize_with = "lenient_object")]
    pictures: RawBannerPictures,
    #[serde(default, deserialize_with = "null_default")]
    site_ids: Vec<u8>,
}

#[derive(Default, Deserialize)]
struct RawBannerPictures {
    #[serde(default)]
    desktop: Option<RawBannerPicture>,
    #[serde(default)]
    mobile: Option<RawBannerPicture>,
}

#[derive(Deserialize)]
struct RawBannerPicture {
    #[serde(default)]
    url: Option<String>,
}

impl From<RawBanner> for Banner {
    fn from(raw: RawBanner) -> Banner {
        Banner {
            id: raw.id,
            url: raw.url,
            desktop: raw.pictures.desktop.and_then(|p| p.url),
            mobile: raw.pictures.mobile.and_then(|p| p.url),
            site_ids: raw.site_ids,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForumCategory {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub color: Option<String>,
}

/// Обсуждение форума на главной.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForumTopic {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    /// К чему привязано обсуждение (`manga`, `anime`), если привязано.
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub source_id: Option<u64>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub likes: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub answered: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_reply_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub owner: Option<UserCard>,
    #[serde(default, deserialize_with = "null_default")]
    pub category: Option<ForumCategory>,
}

/// Оценка отзыва по одному критерию.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewScore {
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    #[serde(default, deserialize_with = "crate::serde_util::f32_lenient")]
    pub value: f32,
}

/// Отзыв на главной. Текст обрезан сервером.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeReview {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    /// Документ ProseMirror как пришёл; форматированный вид даёт `text`.
    #[serde(default)]
    pub content: Value,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub rating: Vec<ReviewScore>,
    /// Тон отзыва: `positive`, `neutral`, `negative`.
    #[serde(default, deserialize_with = "null_default")]
    pub evaluation: Option<Label>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comments_count: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Тайтл, к которому написан отзыв.
    #[serde(default, deserialize_with = "null_default")]
    pub related: Option<TitleShort>,
    #[serde(default, deserialize_with = "null_default")]
    pub user: Option<UserCard>,
}

impl HomeReview {
    pub fn text(&self) -> Doc {
        Doc::from_prosemirror(&self.content)
    }
}

/// Коллекция на главной.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeCollection {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    /// `titles`, `character`, `people`.
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub items_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comments_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub adult: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub spoiler: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Обложки первых элементов.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub previews: Vec<Cover>,
}

/// Вся главная страница одним запросом (`GET /`). Сайт кеширует её на десять минут.
/// Какие блоки показывать и в каком порядке, решают настройки пользователя
/// (`start_page_blocks`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HomePage {
    /// «Обновления популярных тайтлов».
    #[serde(default, deserialize_with = "lenient_vec")]
    pub popular: Vec<FeedTitle>,
    /// «Горячие новинки»: пусто почти всегда.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub hot_newest: Vec<FeedTitle>,
    /// Первая страница «Последних обновлений».
    #[serde(default, deserialize_with = "lenient_vec")]
    pub latest_updates: Vec<FeedTitle>,
    /// «Новинки»: недавно добавленные, без глав.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub newest: Vec<FeedTitle>,
    /// «Сейчас читают».
    #[serde(default, deserialize_with = "null_default")]
    pub currently_views: TopViews,
    /// «Топ активных недели».
    #[serde(default, deserialize_with = "lenient_vec")]
    pub weekly_top_users: Vec<UserCard>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub weekly_top_views_users: Vec<UserCard>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub news: Vec<NewsItem>,
    /// Прочитанные новости: `<id>_<contentVersion>`, см. `NewsItem::visited_key`.
    #[serde(default, deserialize_with = "crate::catalog::string_list")]
    pub visited_news: Vec<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub forum: Vec<ForumTopic>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub reviews: Vec<HomeReview>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub collections: Vec<HomeCollection>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub slider: Vec<Banner>,
}

// ---------------------------------------------------------------------------------------
// Новости
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NewsKind {
    News,
    Changelog,
}

impl NewsKind {
    pub fn param(self) -> &'static str {
        match self {
            NewsKind::News => "news",
            NewsKind::Changelog => "changelog",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NewsAudience {
    All,
    /// Только по ссылке; видно модераторам.
    LinkOnly,
    Team,
    TeamPaid,
}

impl NewsAudience {
    pub fn param(self) -> &'static str {
        match self {
            NewsAudience::All => "all",
            NewsAudience::LinkOnly => "link_only",
            NewsAudience::Team => "team",
            NewsAudience::TeamPaid => "team_paid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NewsPlatform {
    All,
    Android,
    Ios,
    Web,
}

impl NewsPlatform {
    pub fn param(self) -> &'static str {
        match self {
            NewsPlatform::All => "all",
            NewsPlatform::Android => "android",
            NewsPlatform::Ios => "ios",
            NewsPlatform::Web => "web",
        }
    }
}

/// Отбор новостей. Пустые списки не отправляются, и сервер отдаёт всё.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewsFilter {
    pub kinds: Vec<NewsKind>,
    pub audience: Vec<NewsAudience>,
    pub platforms: Vec<NewsPlatform>,
}

impl NewsFilter {
    pub fn to_query(&self, page: u32) -> Vec<(String, String)> {
        let mut p = vec![("page".to_string(), page.max(1).to_string())];
        p.extend(self.kinds.iter().map(|v| ("type[]".to_string(), v.param().to_string())));
        p.extend(
            self.audience
                .iter()
                .map(|v| ("visible_for[]".to_string(), v.param().to_string())),
        );
        p.extend(
            self.platforms
                .iter()
                .map(|v| ("platform[]".to_string(), v.param().to_string())),
        );
        p
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewsMeta {
    /// `disabled` или `enabled`.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub comments: Option<String>,
    /// Версия текста: приходит то строкой, то числом.
    #[serde(default, rename = "contentVersion", deserialize_with = "string_lenient")]
    pub content_version: String,
}

/// Строка списка новостей.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    /// Путь для запросов: `308--versiia-204`. По голому id сервер отвечает 404.
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub status: Option<Label>,
    #[serde(default, deserialize_with = "null_default")]
    pub platform: Option<Label>,
    #[serde(default, deserialize_with = "null_default")]
    pub visible_for: Option<Label>,
    /// `news` или `changelog`.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub kind: Option<Label>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub version_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_object")]
    pub metadata: NewsMeta,
}

impl NewsItem {
    /// Ключ из `HomePage::visited_news`.
    pub fn visited_key(&self) -> String {
        format!("{}_{}", self.id, self.metadata.content_version)
    }
}

/// Файл, приложенный к новости.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsAttachment {
    /// Имя без расширения: по нему картинки в тексте ссылаются на вложение.
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub filename: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub extension: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub width: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub height: Option<u64>,
    /// Пути относительные: `/uploads/news/308/….gif`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub url: String,
    #[serde(default, rename = "previewUrl", deserialize_with = "string_lenient")]
    pub preview_url: String,
    #[serde(default, rename = "originalUrl", deserialize_with = "string_lenient")]
    pub original_url: String,
}

/// Новость целиком.
#[derive(Debug, Clone)]
pub struct NewsArticle {
    pub item: NewsItem,
    /// Текст с подставленными адресами картинок.
    pub content: Doc,
    /// Документ ProseMirror как пришёл, картинки в нём ссылаются на вложения по имени.
    pub content_raw: Value,
    pub attachments: Vec<NewsAttachment>,
    pub author: Option<UserCard>,
}

#[derive(Deserialize)]
struct RawArticle {
    #[serde(flatten)]
    item: NewsItem,
    #[serde(default)]
    content: Value,
    #[serde(default, deserialize_with = "lenient_vec")]
    attachments: Vec<NewsAttachment>,
    #[serde(default, deserialize_with = "null_default")]
    user: Option<UserCard>,
}

/// Подставляет в узлы `image` адреса вложений. Изображение без вложения выбрасывается:
/// иначе в документ попало бы имя файла вместо адреса.
fn resolve_images(node: &mut Value, attachments: &[NewsAttachment], absolute: &dyn Fn(&str) -> String) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(|n| resolve_images(n, attachments, absolute)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && let Some(images) = map
                    .get_mut("attrs")
                    .and_then(|a| a.get_mut("images"))
                    .and_then(Value::as_array_mut)
            {
                for image in images.iter_mut() {
                    let name = image.get("image").and_then(Value::as_str).unwrap_or_default();
                    *image = match attachments.iter().find(|a| a.name == name) {
                        Some(a) => {
                            let preview = [&a.preview_url, &a.url, &a.original_url]
                                .into_iter()
                                .find(|u| !u.is_empty());
                            let original = [&a.original_url, &a.url].into_iter().find(|u| !u.is_empty());
                            match preview {
                                Some(preview) => serde_json::json!({
                                    "url": absolute(preview),
                                    "originalUrl": original.map(|u| absolute(u)),
                                    "width": a.width,
                                    "height": a.height,
                                }),
                                None => Value::Null,
                            }
                        }
                        None => Value::Null,
                    };
                }
            }
            map.values_mut().for_each(|n| resolve_images(n, attachments, absolute));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------
// Запросы
// ---------------------------------------------------------------------------------------

impl Client {
    /// Адрес файла из `/uploads/...` (картинки новостей и баннеров): он лежит на
    /// хосте API, а не на CDN обложек. Абсолютный адрес возвращается как есть.
    pub fn upload_url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        let base = self.api_base();
        let origin = base.trim_end_matches('/').trim_end_matches("/api");
        format!("{origin}/{}", path.trim_start_matches('/'))
    }

    /// Главная страница со всеми блоками. Зависит от сайта.
    pub async fn home(&self, site: Site) -> Result<HomePage> {
        self.get_data(site, "", &()).await
    }

    /// Последние обновления сайта, по 15 тайтлов. `q` и `site_id[]` сервер игнорирует.
    pub async fn latest_updates(&self, site: Site, page: u32) -> Result<Page<FeedTitle>> {
        self.feed_page(site, "latest-updates", &[("page", page.max(1).to_string())])
            .await
    }

    /// Обновления тайтлов из закладок пользователя. Нужен вход. Какие закладки
    /// считаются, задаёт настройка `latest_updates` аккаунта.
    pub async fn user_latest_updates(&self, site: Site, page: u32) -> Result<Page<FeedTitle>> {
        self.feed_page(site, "user-latest-updates", &[("page", page.max(1).to_string())])
            .await
    }

    /// «Сейчас читают» группами по три тайтла.
    pub async fn top_views(&self, site: Site, time: TopTime) -> Result<TopViews> {
        self.get_data(site, "media/top-views", &[("time", time.param())]).await
    }

    /// Одна группа «Сейчас читают» плоским списком по 56, с оценками. Группа манги
    /// на AnimeLib и обратно – ошибка: сервер ответил бы пустым списком.
    pub async fn top_views_page(
        &self,
        site: Site,
        time: TopTime,
        group: TopGroup,
        page: u32,
    ) -> Result<Page<FeedTitle>> {
        if group.is_anime() != site.is_anime() {
            return Err(Error::Other(format!(
                "группа «{}» не относится к {}",
                group.label(),
                site.name()
            )));
        }
        let query = [
            ("time", time.param().to_string()),
            ("popularity", group.id().to_string()),
            ("page", page.max(1).to_string()),
        ];
        self.feed_page(site, "media/top-views", &query).await
    }

    /// «Горячие новинки». Пусто на всех проверенных сайтах.
    pub async fn hot_newest(&self, site: Site, page: u32) -> Result<Page<FeedTitle>> {
        self.feed_page(site, "media/hot-newest", &[("page", page.max(1).to_string())])
            .await
    }

    async fn feed_page(&self, site: Site, path: &str, query: &[(&str, String)]) -> Result<Page<FeedTitle>> {
        let raw: RawList<FeedTitle> = self.get_json(site, path, query).await?;
        Ok(raw.into_page())
    }

    /// Список новостей по 20. Для сайта 1 их около двухсот, для AnimeLib около двадцати.
    pub async fn news(&self, site: Site, filter: &NewsFilter, page: u32) -> Result<Page<NewsItem>> {
        let raw: RawList<NewsItem> = self.get_json(site, "news", &filter.to_query(page)).await?;
        Ok(raw.into_page())
    }

    /// Новость по `slug_url`. Текст приходит документом ProseMirror.
    pub async fn news_article(&self, site: Site, slug_url: &str) -> Result<NewsArticle> {
        let raw: RawArticle = self.get_data(site, &format!("news/{slug_url}"), &()).await?;
        let mut content = raw.content.clone();
        resolve_images(&mut content, &raw.attachments, &|p| self.upload_url(p));
        Ok(NewsArticle {
            item: raw.item,
            content: Doc::from_prosemirror(&content),
            content_raw: raw.content,
            attachments: raw.attachments,
            author: raw.user,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn news_filter_query() {
        assert_eq!(
            NewsFilter::default().to_query(0),
            vec![("page".to_string(), "1".to_string())]
        );
        let q = NewsFilter {
            kinds: vec![NewsKind::News, NewsKind::Changelog],
            audience: vec![NewsAudience::All],
            platforms: vec![NewsPlatform::Web],
        }
        .to_query(2);
        for (k, v) in [
            ("type[]", "news"),
            ("type[]", "changelog"),
            ("visible_for[]", "all"),
            ("platform[]", "web"),
            ("page", "2"),
        ] {
            assert!(q.iter().any(|(a, b)| a == k && b == v), "{k}={v}");
        }
    }

    #[test]
    fn top_groups_belong_to_one_kind() {
        for group in TopGroup::MANGA {
            assert!(!group.is_anime());
            assert_eq!(TopGroup::from_id(group.id()), Some(group));
        }
        for group in TopGroup::ANIME {
            assert!(group.is_anime());
            assert_eq!(TopGroup::from_id(group.id()), Some(group));
        }
        assert_eq!(TopGroup::for_site(Site::AnimeLib), &TopGroup::ANIME);
        assert_eq!(TopGroup::for_site(Site::HentaiLib), &TopGroup::MANGA);
    }

    #[test]
    fn feed_title_with_last_and_latest_items() {
        let popular: FeedTitle = serde_json::from_value(json!({
            "id": 1, "slug_url": "1--a", "name": "A", "site": 1,
            "last_item_at": "2026-10-13T17:29:54.000000Z",
            "metadata": {"last_item": {"id": 5, "model": "chapter", "volume": "7", "number": "49",
                "restricted_view": {"is_open": false, "price": null, "expired_at": null}}}
        }))
        .unwrap();
        assert_eq!(popular.items.len(), 1);
        assert_eq!(popular.items[0].number, "49");
        assert!(!popular.items[0].is_restricted());

        let latest: FeedTitle = serde_json::from_value(json!({
            "id": 2, "slug_url": "2--b", "name": "B", "site": 5,
            "metadata": {"latest_items": {"count": 2, "items": [
                {"id": 9, "model": "episodes", "number": "11", "season": 1, "players": [
                    {"player": "Animelib", "translation_type": {"id": 2, "label": "Озвучка"},
                     "team": {"id": 7, "name": "Команда"}, "views": "3"}]},
                {"id": 8, "model": "episodes", "number": "10"}
            ]}},
            "items_count": {"uploaded": null, "total": 12}
        }))
        .unwrap();
        assert_eq!(latest.items.len(), 2);
        assert!(latest.items[0].is_episode());
        assert_eq!(latest.items[0].season.as_deref(), Some("1"));
        assert_eq!(latest.items[0].players[0].views, 3);
        assert_eq!(latest.items_count.unwrap().total, Some(12));

        let early: FeedItem = serde_json::from_value(json!({
            "id": 1, "number": "3", "restricted_view": {"is_open": false, "expired_at": "2026-10-20T00:00:00.000000Z"}
        }))
        .unwrap();
        assert!(early.is_restricted());
    }

    #[test]
    fn feed_title_without_metadata_still_parses() {
        let title: FeedTitle = serde_json::from_value(json!({"id": 3, "slug_url": "3--c", "metadata": []})).unwrap();
        assert!(title.items.is_empty());
        let top: FeedTitle = serde_json::from_value(json!({"id": 3, "slug_url": "3--c", "views_week": 100})).unwrap();
        assert_eq!(top.views, Some(100));
    }

    #[test]
    fn top_views_groups_are_sorted_numerically() {
        let top: TopViews = serde_json::from_value(json!({
            "time_filter": "day",
            "items": {
                "3": [{"views": 5, "media": {"id": 1, "slug_url": "1--a"}}],
                "21": [],
                "2": [{"views": 7, "media": {"id": 2, "slug_url": "2--b"}}, {"broken": true}]
            }
        }))
        .unwrap();
        let ids: Vec<u32> = top.groups.iter().map(|g| g.popularity).collect();
        assert_eq!(ids, vec![2, 3, 21]);
        assert_eq!(top.groups[0].entries.len(), 1);
        let empty: TopViews = serde_json::from_value(json!({"time_filter": "week", "items": []})).unwrap();
        assert!(empty.groups.is_empty());
        assert_eq!(empty.time, "week");
    }

    #[test]
    fn news_item_content_version_may_be_number() {
        let item: NewsItem = serde_json::from_value(json!({
            "id": 308, "slug_url": "308--v", "title": "Версия",
            "type": {"id": "changelog", "label": "Обновление"},
            "metadata": {"comments": "disabled", "contentVersion": 2}
        }))
        .unwrap();
        assert_eq!(item.visited_key(), "308_2");
        assert_eq!(item.kind.unwrap().id, "changelog");
    }

    #[test]
    fn images_are_resolved_from_attachments() {
        let mut content = json!({"type": "doc", "content": [
            {"type": "image", "attrs": {"images": [{"image": "aa"}, {"image": "нет"}]}}
        ]});
        let attachments = vec![NewsAttachment {
            name: "aa".into(),
            filename: "aa.gif".into(),
            extension: "gif".into(),
            width: Some(800),
            height: Some(600),
            url: "/uploads/news/1/aa.gif".into(),
            preview_url: "/uploads/news/1/aa.gif".into(),
            original_url: "/uploads/news/1/aa.gif".into(),
        }];
        resolve_images(&mut content, &attachments, &|p| format!("https://h{p}"));
        let doc = Doc::from_prosemirror(&content);
        match &doc.blocks[0] {
            crate::rich::Block::Images(pictures) => {
                assert_eq!(pictures.len(), 1);
                assert_eq!(pictures[0].url, "https://h/uploads/news/1/aa.gif");
                assert_eq!(pictures[0].width, Some(800));
            }
            other => panic!("{other:?}"),
        }
    }
}
