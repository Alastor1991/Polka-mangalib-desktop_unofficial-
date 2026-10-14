//! Пользователи: профиль, статистика, история чтения и просмотра, список пользователей
//! и настройки аккаунта (приватность, уведомления, фильтр контента, персонализация).
//!
//! Все запросы общие для сети, кроме истории и статистики профиля: набор записей там
//! зависит от `Site-Id`. Закрытый профиль отдаёт урезанный набор полей, поэтому почти
//! всё в моделях необязательное. Настройки сайт всегда пишет целым объектом, а не
//! отдельными полями, и сервер ждёт неизвестные клиенту поля обратно: модели настроек
//! хранят их в `extra` и возвращают при сохранении.

use std::collections::BTreeMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::error::Result;
use crate::listing::{Page, SortOrder, bool_lenient, opt_bool_lenient, site_ids_lenient};
use crate::models::{Cover, TitleShort};
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;

const PLACEHOLDER_AVATAR: &str = "/placeholders/user_avatar";

/// Аватар или фон. У пользователя без картинки `filename` равен `null`, а `url`
/// ведёт на общую заглушку.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserImage {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

impl UserImage {
    /// Картинка не загружена, `url` ведёт на заглушку сервера.
    pub fn is_placeholder(&self) -> bool {
        self.filename.is_none() || self.url.as_deref().is_some_and(|u| u.contains(PLACEHOLDER_AVATAR))
    }
}

/// «Серия посещений». У скрытых пользователей от неё остаётся только `last_login_at`
/// со значением `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginStreak {
    /// Дата без `T`, в отличие от остальных: `2026-10-13 03:54:30`.
    #[serde(default)]
    pub last_login_at: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub login_streak: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub max_login_streak: u64,
}

/// Карточка пользователя, одинаковая во всех ответах сервиса.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserRef {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<UserImage>,
    /// Время последнего посещения. У закрытых профилей `None`.
    #[serde(default)]
    pub last_online_at: Option<String>,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub can_view_profile: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub can_view_statistics: bool,
    /// Сайты, статистику которых владелец разрешил смотреть.
    #[serde(default, deserialize_with = "site_ids_lenient")]
    pub viewable_statistics_site_ids: Vec<u8>,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub can_view_previous_usernames: bool,
    #[serde(default)]
    pub login_streak: Option<LoginStreak>,
    #[serde(default)]
    pub login_streak_preferences: Option<Value>,
    #[serde(default)]
    pub premium: Option<Value>,
}

impl UserRef {
    /// Адрес аватара, если пользователь его загрузил.
    pub fn avatar_url(&self) -> Option<&str> {
        let avatar = self.avatar.as_ref()?;
        if avatar.is_placeholder() {
            None
        } else {
            avatar.url.as_deref()
        }
    }

    pub fn has_premium(&self) -> bool {
        self.premium
            .as_ref()
            .and_then(|p| p.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// Уровень и очки. Без `top` пользователь вне топа.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PointsInfo {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub top: Option<u64>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total_points: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub level: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub max_level_points: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub current_level_points: u64,
    #[serde(default)]
    pub point_percent_progress: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gender {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
}

/// Команда в профиле: коротко, без счётчиков.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserTeam {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub slug_url: Option<String>,
    #[serde(default)]
    pub cover: Option<Cover>,
}

/// Бывший ник пользователя.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousUsername {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
    #[serde(default)]
    pub changed_at: Option<String>,
}

/// Дополнительные поля профиля (`fields[]`). Без них приходит только `UserRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileField {
    Background,
    Roles,
    Points,
    BanInfo,
    Gender,
    CreatedAt,
    About,
    Teams,
    PremiumBackgroundId,
    LoginStreak,
    PreviousUsernames,
    AvatarFrameId,
    /// Следующие четыре сервер отдаёт только владельцу аккаунта.
    Email,
    Confirmed,
    Oauth,
    Balance,
}

impl ProfileField {
    /// Всё, что показывает страница чужого профиля.
    pub const PROFILE_PAGE: [ProfileField; 10] = [
        ProfileField::Background,
        ProfileField::Roles,
        ProfileField::Points,
        ProfileField::BanInfo,
        ProfileField::Gender,
        ProfileField::CreatedAt,
        ProfileField::About,
        ProfileField::Teams,
        ProfileField::PremiumBackgroundId,
        ProfileField::LoginStreak,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProfileField::Background => "background",
            ProfileField::Roles => "roles",
            ProfileField::Points => "points",
            ProfileField::BanInfo => "ban_info",
            ProfileField::Gender => "gender",
            ProfileField::CreatedAt => "created_at",
            ProfileField::About => "about",
            ProfileField::Teams => "teams",
            ProfileField::PremiumBackgroundId => "premium_background_id",
            ProfileField::LoginStreak => "login_streak",
            ProfileField::PreviousUsernames => "previous_usernames",
            ProfileField::AvatarFrameId => "avatar_frame_id",
            ProfileField::Email => "email",
            ProfileField::Confirmed => "confirmed",
            ProfileField::Oauth => "oauth",
            ProfileField::Balance => "balance",
        }
    }
}

/// Профиль пользователя. Что из необязательного пришло, зависит от запрошенных
/// `ProfileField` и от приватности: у закрытого профиля поля просто отсутствуют.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserProfile {
    #[serde(flatten)]
    pub user: UserRef,
    #[serde(default)]
    pub background: Option<UserImage>,
    /// Описание «о себе», как есть. Разобранное – `UserProfile::about_text`.
    #[serde(default)]
    pub about: Option<Value>,
    #[serde(default)]
    pub gender: Option<Gender>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Блокировки: id, срок, причина. Структура записи не разбиралась.
    #[serde(default, deserialize_with = "null_default")]
    pub ban_info: Vec<Value>,
    #[serde(default)]
    pub points_info: Option<PointsInfo>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub teams: Vec<UserTeam>,
    #[serde(default, deserialize_with = "null_default")]
    pub roles: Vec<Value>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub previous_usernames: Vec<PreviousUsername>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub email: Option<String>,
}

impl UserProfile {
    /// Описание «о себе». Приходит то обычной строкой, то документом редактора.
    pub fn about_text(&self) -> Doc {
        match &self.about {
            Some(Value::String(s)) if s.trim_start().starts_with('<') => Doc::from_html(s),
            Some(Value::String(s)) => Doc::from_plain(s),
            Some(v) => Doc::from_prosemirror(v),
            None => Doc::default(),
        }
    }

    pub fn is_banned(&self) -> bool {
        !self.ban_info.is_empty()
    }
}

/// Число со склонённой подписью, как его показывает сайт.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatValue {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
    /// Готовая строка для показа: `1 234`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub formated: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub short: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    /// Доля от всего в процентах. Есть у жанров и тегов.
    #[serde(default)]
    pub percent: Option<f32>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewStats {
    /// Виды отзывов, как есть.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub kinds: Vec<Value>,
    /// Счётчики оценок: `neutral`, `positive`, `negative`, `all`.
    #[serde(default, deserialize_with = "null_default")]
    pub evaluation: BTreeMap<String, u64>,
}

/// Статистика профиля. Набор ключей зависит от сайта, у аниме он другой.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserStats {
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub genres: Vec<StatValue>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub tags: Vec<StatValue>,
    #[serde(default)]
    pub manga_added: Option<StatValue>,
    #[serde(default)]
    pub chapters_added: Option<StatValue>,
    #[serde(default)]
    pub comments: Option<StatValue>,
    #[serde(default)]
    pub reviews: Option<ReviewStats>,
}

/// Сколько глав и серий просмотрено.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewTotals {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub manga_chapters: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub ranobe_chapters: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub anime_episodes: u64,
}

/// Потраченное время. Минуты прикидочные: сервер умножает число глав и серий на
/// `ViewStatistics::minutes_per_item`. Строки готовы к показу по-русски.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewTime {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub manga_minutes: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub ranobe_minutes: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub anime_minutes: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total_minutes: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub manga: String,
    #[serde(default, deserialize_with = "null_default")]
    pub ranobe: String,
    #[serde(default, deserialize_with = "null_default")]
    pub anime: String,
    #[serde(default, deserialize_with = "null_default")]
    pub total: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewPeriod {
    #[serde(default, deserialize_with = "null_default")]
    pub totals: ViewTotals,
    #[serde(default, deserialize_with = "null_default")]
    pub time: ViewTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewPeriods {
    #[serde(default, deserialize_with = "null_default")]
    pub last_7_days: ViewPeriod,
    #[serde(default, deserialize_with = "null_default")]
    pub last_30_days: ViewPeriod,
    #[serde(default, deserialize_with = "null_default")]
    pub last_365_days: ViewPeriod,
    #[serde(default, deserialize_with = "null_default")]
    pub all_time: ViewPeriod,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartSeries {
    #[serde(default, deserialize_with = "null_default")]
    pub key: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    /// Цвет ряда, `#rrggbb`.
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub data: Vec<u64>,
}

/// График за период: подписи по оси X и ряды.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewChart {
    #[serde(default, deserialize_with = "null_default")]
    pub labels: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub datasets: Vec<ChartSeries>,
}

/// Статистика чтения и просмотра.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewStatistics {
    #[serde(default, deserialize_with = "site_ids_lenient")]
    pub site_ids: Vec<u8>,
    #[serde(default, deserialize_with = "null_default")]
    pub totals: ViewTotals,
    #[serde(default, deserialize_with = "null_default")]
    pub time: ViewTime,
    #[serde(default, deserialize_with = "null_default")]
    pub periods: ViewPeriods,
    /// Графики по ключам `last_7_days`, `last_30_days`, `last_365_days`.
    #[serde(default, deserialize_with = "null_default")]
    pub charts: BTreeMap<String, ViewChart>,
    /// На сколько минут сервер умножает одну главу или серию: `manga`, `ranobe`, `anime`.
    #[serde(default, deserialize_with = "null_default")]
    pub minutes_per_item: BTreeMap<String, u64>,
}

/// Какую историю смотреть.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// Прочитанные главы манги и ранобэ. Набор зависит от `Site-Id`.
    Chapters,
    /// «Озвучка манги». Без озвучки на аккаунте отвечает так же, как `Chapters`.
    ChapterPlayers,
    /// Просмотренные серии аниме. Всегда от имени AnimeLib.
    Anime,
}

impl HistoryKind {
    fn path(self) -> &'static str {
        match self {
            HistoryKind::Chapters => "user/chapters/history",
            HistoryKind::ChapterPlayers => "user/chapters/players/history",
            HistoryKind::Anime => "user/players/history",
        }
    }
}

/// Глава или серия, на которой остановились.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryItem {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    /// `chapter` или `episode`.
    #[serde(default, deserialize_with = "null_default")]
    pub model: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number_secondary: String,
    /// Название бывает `null`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub branch_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub manga_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub anime_id: Option<u64>,
    /// Сезон серии.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub season: Option<String>,
    /// Плееры серии, как есть.
    #[serde(default, deserialize_with = "null_default")]
    pub players: Vec<Value>,
}

/// Запись истории.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Когда смотрели, UTC.
    #[serde(default)]
    pub view_at: Option<String>,
    pub media: TitleShort,
    #[serde(default)]
    pub item: Option<HistoryItem>,
}

/// Вид общего списка пользователей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsersFilter {
    WeeklyTop,
    /// Топ-100 по очкам. Весь список приходит за раз, без пагинации.
    Points,
    Moderators,
    /// Серия посещений. Нужен `UsersQuery::login_streak`.
    DailyStreak,
    WeeklyViews,
}

impl UsersFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            UsersFilter::WeeklyTop => "weekly-top",
            UsersFilter::Points => "points",
            UsersFilter::Moderators => "moderators",
            UsersFilter::DailyStreak => "daily-streak",
            UsersFilter::WeeklyViews => "weekly-views",
        }
    }
}

/// Запрос к списку пользователей. Для общего списка `filter` оставляют пустым: значение
/// `all` сервер отвергает с 422.
#[derive(Debug, Clone, Default)]
pub struct UsersQuery {
    pub page: u32,
    pub filter: Option<UsersFilter>,
    /// Только для `UsersFilter::DailyStreak`: длина серии.
    pub login_streak: Option<u64>,
    /// Порядок по `id`. Без значения сервер выбирает сам.
    pub order: Option<SortOrder>,
    /// Поиск по нику, от двух символов.
    pub text: String,
}

/// Пользователь в списке: карточка и то, что зависит от вида списка.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListedUser {
    #[serde(flatten)]
    pub user: UserRef,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub points_info: Option<PointsInfo>,
    /// Только в топе недели.
    #[serde(default)]
    pub weekly_views_progress: Option<Value>,
    /// Только у модераторов.
    #[serde(default, deserialize_with = "null_default")]
    pub roles: Vec<Value>,
}

/// Кому видна часть профиля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    FriendsOnly,
    /// Всем, кроме игнор-листа. Для старых ников значения нет.
    ExceptIgnored,
    Private,
    /// Значение, которого клиент не знает: сохраняется как есть.
    Other(u64),
}

impl Visibility {
    pub fn code(self) -> u64 {
        match self {
            Visibility::Public => 0,
            Visibility::FriendsOnly => 1,
            Visibility::ExceptIgnored => 2,
            Visibility::Private => 3,
            Visibility::Other(n) => n,
        }
    }

    pub fn from_code(code: u64) -> Visibility {
        match code {
            0 => Visibility::Public,
            1 => Visibility::FriendsOnly,
            2 => Visibility::ExceptIgnored,
            3 => Visibility::Private,
            n => Visibility::Other(n),
        }
    }
}

impl Serialize for Visibility {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u64(self.code())
    }
}

impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Visibility::from_code(u64_lenient(d)?))
    }
}

/// Настройки приватности. Сайт отправляет объект целиком, как получил.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacySettings {
    pub profile_visibility: Visibility,
    pub statistics_visibility: Visibility,
    /// Сайты, статистика которых видна. Числа бывают строками, список бывает объектом.
    #[serde(default, deserialize_with = "site_ids_lenient")]
    pub statistics_site_ids: Vec<u8>,
    /// У старых ников значения `ExceptIgnored` нет.
    pub previous_usernames_visibility: Visibility,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub previous_usernames: Vec<PreviousUsername>,
}

impl Default for PrivacySettings {
    /// Значения, которые сайт подставляет, когда сервер ничего не прислал.
    fn default() -> Self {
        PrivacySettings {
            profile_visibility: Visibility::Public,
            statistics_visibility: Visibility::Private,
            statistics_site_ids: Site::ALL.iter().map(|s| s.id()).collect(),
            previous_usernames_visibility: Visibility::Public,
            previous_usernames: Vec::new(),
        }
    }
}

impl PrivacySettings {
    /// Убирает ник из истории. Сервер удаляет его, когда в сохранённом списке его нет.
    pub fn forget_username(&mut self, id: u64) {
        self.previous_usernames.retain(|u| u.id != id);
    }
}

/// Оповещения аккаунта. Сайт правит шесть полей, остальные возвращает как получил.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(
        default,
        deserialize_with = "opt_u64_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_id: Option<u64>,
    /// Оповещать о новых тайтлах на сайте.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub manga: bool,
    /// Инверсия: `true` значит заявки в друзья не оповещаются.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub disable_friends_notif: bool,
    /// Оповещать о завершении тайтла из моих списков.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub media_status_finished: bool,
    /// Не оповещать об ответах под комментариями старше этого числа дней: 7, 14, 30,
    /// 180 или 360. `None` значит выключено. Сервер присылает `false`, а ждёт `0`.
    #[serde(
        default,
        deserialize_with = "old_comments_days",
        serialize_with = "serialize_old_comments_days"
    )]
    pub disable_old_comments_notif: Option<u32>,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub disable_chapter_early_access_notif: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub disable_card_drop_notif: bool,
    /// Следующие пять поля соответствуют папкам закладок по умолчанию.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub reading: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub plan_read: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub on_hold: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub completed: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub favourites: bool,
    /// Браузерные пуши. Страница настроек сайта их не меняет.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub push_chapter: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub push_comments: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub push_episode: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub push_forum: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub push_messages: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub notify_anime: bool,
    /// Поля, которых клиент не знает. Уходят обратно без изменений.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn old_comments_days<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<u32>, D::Error> {
    Ok(opt_u64_lenient(d)?
        .filter(|n| *n > 0)
        .and_then(|n| u32::try_from(n).ok()))
}

fn serialize_old_comments_days<S: serde::Serializer>(v: &Option<u32>, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_u32(v.unwrap_or(0))
}

/// Что фильтруется.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterKind {
    Genre,
    Tag,
    /// Тип тайтла: манга, манхва, ранобэ.
    Type,
}

/// Правило фильтра контента: этот жанр, тег или тип скрывается на этом сайте. Одно и то
/// же правило на двух сайтах – две записи. Правила с неизвестным видом клиент выбрасывает.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentFilterRule {
    /// Id из справочника `constants` (`genres`, `tags`, `types`).
    #[serde(deserialize_with = "u64_lenient")]
    pub value: u64,
    #[serde(rename = "type")]
    pub kind: FilterKind,
    #[serde(deserialize_with = "site_id_lenient")]
    pub site_id: u8,
}

fn site_id_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u8, D::Error> {
    Ok(u8::try_from(u64_lenient(d)?).unwrap_or(0))
}

/// Блок стартовой страницы: `id` из `popular-updates`, `latest-views`, `top-views`,
/// `forum`, `reviews`, `collections`, `latest-updates`, `top-users`, `newest`,
/// `hot-newest` (на AnimeLib его нет). Порядок в списке – порядок на странице.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartPageBlock {
    pub id: String,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub visible: bool,
}

/// Настройки внешнего вида. Допустимые значения сервер перечисляет в `constants`
/// (`prefs*`): `latest_updates` – `notification`, `all_except_abandoned`, `all`;
/// `grid_cards_count` – `auto`, `2`, `3`, `4`; `media_info_variant` – `standard`,
/// `compact`, `list`; `login_streak_theme` – `auto`, `light`, `dark`. Остальное – см.
/// `docs/api.md`. Поля, которых нет в ответе, остаются `None` и в запрос не попадают.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersonalizationSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_updates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom_menu_visible: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_string_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub grid_cards_count: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_page: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub start_page_blocks: Vec<StartPageBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_info_variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_info_tooltip: Option<String>,
    /// Перестановка `bookmarks`, `catalog`, `home`, `notifications`, `menu`.
    #[serde(default, deserialize_with = "null_default")]
    pub bottom_menu_order: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom_menu_button_variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_bottom_navigation_catalog_visible: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_bottom_navigation_catalog_close_action: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_bool_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub login_streak_comments_hidden: Option<bool>,
    /// Стили с префиксом `premium-` доступны только с премиумом.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_streak_light_icon_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_streak_dark_icon_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_streak_theme: Option<String>,
    #[serde(
        default,
        deserialize_with = "opt_bool_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub login_streak_others_personalization_disabled: Option<bool>,
    /// Если `false`, сайт сам кладёт тайтл в закладки, когда начинаешь читать. Клиенту,
    /// который ведёт закладки сам, это стоит знать.
    #[serde(
        default,
        deserialize_with = "opt_bool_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub disable_autobookmark: Option<bool>,
    /// Поля, которых клиент не знает.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Client {
    /// Профиль. Без `fields` приходит только карточка. Для несуществующего
    /// пользователя `Error::NotFound`.
    pub async fn user_profile(&self, user_id: u64, fields: &[ProfileField]) -> Result<UserProfile> {
        let query: Vec<(&str, &str)> = fields.iter().map(|f| ("fields[]", f.as_str())).collect();
        self.get_data(Site::MangaLib, &format!("user/{user_id}"), &query).await
    }

    /// Статистика профиля: жанры, теги, добавленное, комментарии, отзывы. Набор ключей
    /// зависит от сайта.
    pub async fn user_stats(&self, site: Site, user_id: u64) -> Result<UserStats> {
        self.get_data(site, &format!("user/{user_id}/stats"), &()).await
    }

    /// Статистика чтения и просмотра по сайтам. Без `sites` сервер берёт сайт запроса.
    /// Закрытая статистика даёт `Error::Forbidden`.
    pub async fn user_view_statistics(&self, user_id: u64, sites: &[Site]) -> Result<ViewStatistics> {
        let query: Vec<(&str, String)> = sites.iter().map(|s| ("site_ids[]", s.id().to_string())).collect();
        self.get_data(Site::MangaLib, &format!("user/{user_id}/view-statistics"), &query)
            .await
    }

    /// История вошедшего пользователя, по 40 записей, новые сверху. У страниц нет
    /// общего числа: листать надо, пока `has_next`. `site` задаёт сайт для манги и
    /// ранобэ, для аниме он всегда AnimeLib.
    pub async fn view_history(&self, kind: HistoryKind, site: Site, page: u32) -> Result<Page<HistoryEntry>> {
        let site = if kind == HistoryKind::Anime {
            Site::AnimeLib
        } else {
            site
        };
        self.get_page(site, kind.path(), &[("page", page.max(1).to_string())])
            .await
    }

    /// Список пользователей и топы. `UsersFilter::Points` отдаёт все сто записей
    /// сразу, у `Page` в этом случае нет ни `total`, ни `has_next`.
    pub async fn users(&self, q: &UsersQuery) -> Result<Page<ListedUser>> {
        let mut query: Vec<(&str, String)> = vec![("page", q.page.max(1).to_string())];
        if let Some(order) = q.order {
            query.push(("sort_by", "id".into()));
            query.push(("sort_type", order.as_str().into()));
        }
        if let Some(filter) = q.filter {
            query.push(("filter", filter.as_str().into()));
            if let (UsersFilter::DailyStreak, Some(days)) = (filter, q.login_streak) {
                query.push(("login_streak", days.to_string()));
            }
        }
        let text = q.text.trim();
        if text.chars().count() >= 2 {
            query.push(("q", text.to_string()));
        }
        self.get_page(Site::MangaLib, "user", &query).await
    }

    /// Настройки приватности вошедшего пользователя.
    pub async fn privacy_settings(&self) -> Result<PrivacySettings> {
        self.get_data(Site::MangaLib, "user/settings/privacy", &()).await
    }

    /// Сохраняет приватность. Отправляйте объект, полученный из `privacy_settings`,
    /// с изменёнными полями: сервер ждёт его целиком.
    pub async fn set_privacy_settings(&self, settings: &PrivacySettings) -> Result<()> {
        self.send_ok(Site::MangaLib, Method::PUT, "user/settings/privacy", Some(settings))
            .await
    }

    pub async fn notification_settings(&self) -> Result<NotificationSettings> {
        self.get_data(Site::MangaLib, "user/settings/notifications", &()).await
    }

    /// Сохраняет настройки оповещений целым объектом.
    pub async fn set_notification_settings(&self, settings: &NotificationSettings) -> Result<()> {
        self.send_ok(
            Site::MangaLib,
            Method::PUT,
            "user/settings/notifications",
            Some(settings),
        )
        .await
    }

    /// Правила фильтра контента. Изменения действуют на каталог не сразу: сервер
    /// кэширует их 10–20 минут.
    pub async fn content_filter(&self, user_id: u64) -> Result<Vec<ContentFilterRule>> {
        // Серверу понятен не только список в `data`, но и отсутствие записей.
        let rules: Option<Vec<Value>> = self
            .get_data(Site::MangaLib, &format!("user/{user_id}/content-filter"), &())
            .await?;
        Ok(rules
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Заменяет набор правил целиком: то, чего нет в `rules`, сервер удаляет.
    pub async fn set_content_filter(&self, user_id: u64, rules: &[ContentFilterRule]) -> Result<()> {
        let body = json!({ "filters": rules });
        self.send_ok(
            Site::MangaLib,
            Method::POST,
            &format!("user/{user_id}/content-filter"),
            Some(&body),
        )
        .await
    }

    pub async fn personalization_settings(&self, user_id: u64) -> Result<PersonalizationSettings> {
        self.get_data(Site::MangaLib, &format!("user/{user_id}/personalization-settings"), &())
            .await
    }

    /// Сохраняет персонализацию целым объектом из `personalization_settings`.
    pub async fn set_personalization_settings(&self, user_id: u64, settings: &PersonalizationSettings) -> Result<()> {
        self.send_ok(
            Site::MangaLib,
            Method::POST,
            &format!("user/{user_id}/personalization-settings"),
            Some(settings),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ref_tolerates_gaps() {
        let u: UserRef = serde_json::from_value(json!({
            "id": "1001", "username": "user1",
            "avatar": {"filename": null, "url": "https://cover.cdnlibs.org/static/images/placeholders/user_avatar.png"},
            "login_streak": {"last_login_at": null},
            "viewable_statistics_site_ids": {"0": 1, "1": "5"}
        }))
        .unwrap();
        assert_eq!(u.id, 1001);
        assert!(u.avatar_url().is_none());
        assert_eq!(u.viewable_statistics_site_ids, vec![1, 5]);
        assert!(!u.can_view_profile && !u.has_premium());
    }

    #[test]
    fn notification_settings_roundtrip() {
        let raw = json!({
            "user_id": 1001, "manga": false, "disable_friends_notif": 0, "media_status_finished": true,
            "disable_old_comments_notif": false, "disable_chapter_early_access_notif": false,
            "disable_card_drop_notif": false, "reading": true, "plan_read": false, "on_hold": false,
            "completed": false, "favourites": false, "push_chapter": true, "push_comments": true,
            "push_episode": true, "push_forum": true, "push_messages": true, "notify_anime": true,
            "новое_поле": 5
        });
        let mut s: NotificationSettings = serde_json::from_value(raw).unwrap();
        assert_eq!(s.disable_old_comments_notif, None);
        s.disable_old_comments_notif = Some(30);
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["disable_old_comments_notif"], 30);
        assert_eq!(out["новое_поле"], 5);
        assert_eq!(out["push_forum"], true);
        s.disable_old_comments_notif = None;
        assert_eq!(serde_json::to_value(&s).unwrap()["disable_old_comments_notif"], 0);
    }

    #[test]
    fn visibility_keeps_unknown_codes() {
        let p: PrivacySettings = serde_json::from_value(json!({
            "profile_visibility": 0, "statistics_visibility": "3", "statistics_site_ids": ["1", 2],
            "previous_usernames_visibility": 9,
            "previous_usernames": [{"id": 1, "username": "old", "changed_at": null}, 5]
        }))
        .unwrap();
        assert_eq!(p.statistics_visibility, Visibility::Private);
        assert_eq!(p.previous_usernames_visibility, Visibility::Other(9));
        assert_eq!(p.previous_usernames.len(), 1);
        let out = serde_json::to_value(&p).unwrap();
        assert_eq!(out["previous_usernames_visibility"], 9);
        assert_eq!(out["statistics_site_ids"], json!([1, 2]));
    }
}
