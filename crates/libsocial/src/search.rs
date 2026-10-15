//! Поиск и списки людей, команд, персонажей, издателей, франшиз и пользователей.
//!
//! Вход для чтения не нужен. Тайтлы этих сущностей отдаёт каталог
//! (`CatalogFilter::of`), здесь только сами карточки. Что здесь неочевидно:
//!
//! - `q` короче двух символов даёт 422, поэтому клиент отказывает заранее;
//! - списки без `q` приходят с полной пагинацией (`total`, `last_page`), с `q` у людей,
//!   команд и персонажей она урезается до курсорной: надёжен только `has_next`;
//! - фильтр по сайтам называется `site_ids[]` (с `s`), в каталоге это `site_id[]`;
//! - число тайтлов по сайтам лежит в `titles_count_details`, ключи там номера сайтов;
//! - у издателя бывает пустой `slug`, тогда `slug_url` выглядит как `6848--`, и он
//!   всё равно работает как путь;
//! - `/user` с `filter=points` и `moderators` отдаёт всех сразу: ни `meta`, ни `links`.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::catalog::{SortOrder, lenient_object};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::listing::Page;
use crate::models::Cover;
use crate::rich::Doc;
use crate::serde_util::{lenient_vec, null_default, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;
use crate::users::PointsInfo;

/// Наименьшая длина `q`, которую принимают списки сущностей.
pub const MIN_QUERY_CHARS: usize = 2;

/// Наибольший `limit` у списка персонажей.
pub const MAX_CHARACTER_LIMIT: u32 = 30;

// ---------------------------------------------------------------------------------------
// Сущности: люди, команды, персонажи, издатели, франшизы
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityKind {
    People,
    Team,
    Character,
    Publisher,
    Franchise,
}

impl EntityKind {
    pub const ALL: [EntityKind; 5] = [
        EntityKind::People,
        EntityKind::Team,
        EntityKind::Character,
        EntityKind::Publisher,
        EntityKind::Franchise,
    ];

    /// Путь списка в API. У команд он во множественном числе, у остальных в единственном.
    pub fn path(self) -> &'static str {
        match self {
            EntityKind::People => "people",
            EntityKind::Team => "teams",
            EntityKind::Character => "character",
            EntityKind::Publisher => "publisher",
            EntityKind::Franchise => "franchise",
        }
    }

    /// Значение `model` в карточке и `target_model` в каталоге.
    pub fn model(self) -> &'static str {
        match self {
            EntityKind::People => "people",
            EntityKind::Team => "team",
            EntityKind::Character => "character",
            EntityKind::Publisher => "publisher",
            EntityKind::Franchise => "franchise",
        }
    }

    /// Тот же вид для фильтра каталога по тайтлам сущности.
    pub fn target(self) -> crate::catalog::TargetModel {
        use crate::catalog::TargetModel;
        match self {
            EntityKind::People => TargetModel::People,
            EntityKind::Team => TargetModel::Team,
            EntityKind::Character => TargetModel::Character,
            EntityKind::Publisher => TargetModel::Publisher,
            EntityKind::Franchise => TargetModel::Franchise,
        }
    }

    /// Порядок, который по умолчанию выбирает сайт: людей и персонажей по подписчикам,
    /// команды по лайкам, остальное по имени.
    pub fn site_default_sort(self) -> EntitySort {
        match self {
            EntityKind::People | EntityKind::Character => EntitySort::Subscribers,
            EntityKind::Team => EntitySort::Likes,
            EntityKind::Publisher | EntityKind::Franchise => EntitySort::Name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntitySort {
    Name,
    TitlesCount,
    Subscribers,
    /// Только у команд: у людей сервер отвечает 422.
    Likes,
}

impl EntitySort {
    pub fn param(self) -> &'static str {
        match self {
            EntitySort::Name => "name",
            EntitySort::TitlesCount => "titles_count",
            EntitySort::Subscribers => "subscribes_count",
            EntitySort::Likes => "likes_count",
        }
    }
}

/// Запрос списка сущностей. Пустой запрос даёт весь список в порядке сервера.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityQuery {
    /// Поиск по названию и синонимам, не короче двух символов.
    pub text: String,
    /// `None` – порядок сервера. Сайт ставит `EntityKind::site_default_sort`.
    pub sort: Option<EntitySort>,
    pub order: SortOrder,
    /// Сайты, на которых есть тайтлы сущности (`site_ids[]`). Пусто – любые.
    pub site_ids: Vec<u8>,
    /// Размер страницы. Принимает только список персонажей, 0–30, остальные
    /// списки его игнорируют и сюда не отправляют.
    pub limit: Option<u32>,
}

impl EntityQuery {
    pub fn search(text: impl Into<String>) -> EntityQuery {
        EntityQuery {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn to_query(&self, kind: EntityKind, page: u32) -> Result<Vec<(String, String)>> {
        let text = self.text.trim();
        if !text.is_empty() && text.chars().count() < MIN_QUERY_CHARS {
            return Err(Error::Other(format!(
                "запрос должен содержать не менее {MIN_QUERY_CHARS} символов"
            )));
        }
        if kind == EntityKind::People && self.sort == Some(EntitySort::Likes) {
            return Err(Error::Other("людей нельзя сортировать по лайкам".into()));
        }
        let mut p = vec![("page".to_string(), page.max(1).to_string())];
        if !text.is_empty() {
            p.push(("q".into(), text.to_string()));
        }
        if let Some(sort) = self.sort {
            p.push(("sort_by".into(), sort.param().into()));
            p.push(("sort_type".into(), self.order.param().into()));
        }
        for site in &self.site_ids {
            p.push(("site_ids[]".into(), site.to_string()));
        }
        if kind == EntityKind::Character
            && let Some(limit) = self.limit
        {
            if limit > MAX_CHARACTER_LIMIT {
                return Err(Error::Other(format!(
                    "размер страницы персонажей не больше {MAX_CHARACTER_LIMIT}"
                )));
            }
            p.push(("limit".into(), limit.to_string()));
        }
        Ok(p)
    }
}

/// Подписка вошедшего на сущность. Запись подписки в эту область не входит.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    #[serde(default, deserialize_with = "null_default")]
    pub is_subscribed: bool,
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub source_id: Option<u64>,
    #[serde(default)]
    pub relation: Option<Value>,
}

/// Счётчик в карточке: тайтлы, подписчики, лайки. Подпись склоняется по числу
/// («Тайтл», «Тайтла», «Тайтлов»), поэтому ищут по `tag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    /// `titles`, `subscribes`, `likes`, у деталей команды ещё `chapters`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub tag: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
    /// С разделителями разрядов: `38 666`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub formated: String,
    /// Коротко: `38.7 K`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub short: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
}

/// Карточка человека, команды, персонажа, издателя или франшизы. Состав полей у
/// видов разный, чужие поля остаются пустыми.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityCard {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    /// Путь для запросов: `113727--junji-itou`. Бывает с пустым хвостом (`6848--`).
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    /// `people`, `team`, `character`, `publisher`, `franchise`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub model: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub rus_name: Option<String>,
    /// Прочие имена одной строкой, склеены переводом строки или « / ».
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub alt_name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub cover: Option<Cover>,
    #[serde(default, deserialize_with = "null_default")]
    pub subscription: Option<Subscription>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub stats: Vec<Stat>,
    /// Число тайтлов по сайтам, ключ – номер сайта.
    #[serde(default, deserialize_with = "site_counts")]
    pub titles_count_details: BTreeMap<u8, u64>,
    /// Только у человека: страницу подтвердил сам автор.
    #[serde(default)]
    pub confirmed: Option<bool>,
    /// Только у человека: пользователь, привязанный к странице.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub user_id: Option<u64>,
    /// Описание персонажа и издателя: документ ProseMirror. Разбирайте через `description`.
    #[serde(default)]
    pub dsc: Option<Value>,
    /// Описание команды в детали.
    #[serde(default)]
    pub description: Option<Value>,
    /// Ссылки команды (только в детали).
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub vk: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub discord: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub website: Option<String>,
}

impl EntityCard {
    /// Русское имя, а если его нет, основное.
    pub fn display_name(&self) -> &str {
        self.rus_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    /// Прочие имена по одному.
    pub fn alt_names(&self) -> Vec<&str> {
        self.alt_name
            .as_deref()
            .unwrap_or_default()
            .split(['\n', '/'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Значение счётчика по его `tag`.
    pub fn stat(&self, tag: &str) -> Option<u64> {
        self.stats.iter().find(|s| s.tag == tag).map(|s| s.value)
    }

    pub fn titles_count(&self) -> Option<u64> {
        self.stat("titles")
    }

    pub fn subscribers(&self) -> Option<u64> {
        self.stat("subscribes")
    }

    /// Сколько у сущности тайтлов на сайте с таким номером.
    pub fn titles_on(&self, site: u8) -> u64 {
        self.titles_count_details.get(&site).copied().unwrap_or(0)
    }

    /// Сайты, где есть тайтлы сущности: по ним сайт выбирает вкладку.
    pub fn sites_with_titles(&self) -> Vec<u8> {
        self.titles_count_details
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(site, _)| *site)
            .collect()
    }

    /// Описание как форматированный текст. Пустой документ – описания нет.
    pub fn description(&self) -> Doc {
        let raw = self.dsc.as_ref().or(self.description.as_ref());
        raw.filter(|v| !v.is_null())
            .map(Doc::from_prosemirror)
            .unwrap_or_default()
    }
}

/// `{"1": 48, "3": 0}`. Пустой объект сервер иногда присылает как `[]`.
fn site_counts<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<BTreeMap<u8, u64>, D::Error> {
    let raw: BTreeMap<String, Value> = lenient_object(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(site, count)| {
            let count = match count {
                Value::Number(n) => n.as_u64()?,
                Value::String(s) => s.trim().parse().ok()?,
                _ => return None,
            };
            Some((site.parse().ok()?, count))
        })
        .collect())
}

// ---------------------------------------------------------------------------------------
// Пользователи
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Avatar {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub url: String,
}

impl Avatar {
    /// Картинка-заглушка вместо своей аватарки.
    pub fn is_placeholder(&self) -> bool {
        self.filename.is_none() || self.url.contains("/placeholders/")
    }
}

/// Роль модератора в списке `moderators`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRole {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub rus_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Premium {
    #[serde(default, deserialize_with = "null_default")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginStreak {
    #[serde(default)]
    pub last_login_at: Option<String>,
}

/// Пользователь в списках, лентах главной и подписи автора. Полный профиль и его
/// настройки приходят из других запросов.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCard {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub username: String,
    #[serde(default, deserialize_with = "null_default")]
    pub avatar: Option<Avatar>,
    #[serde(default)]
    pub last_online_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub can_view_profile: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub premium: Premium,
    #[serde(default, deserialize_with = "null_default")]
    pub login_streak: LoginStreak,
    /// Есть в `weekly-top` и `points`, в главной у «Топа активных недели».
    #[serde(default)]
    pub points_info: Option<PointsInfo>,
    /// Есть в `moderators`.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub roles: Vec<UserRole>,
}

impl UserCard {
    pub fn is_premium(&self) -> bool {
        self.premium.enabled
    }
}

/// Какой список пользователей показать.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserFilter {
    /// Все пользователи. Сайт значение `all` не отправляет, и здесь тоже.
    #[default]
    All,
    /// Топ активных недели.
    WeeklyTop,
    /// Топ 100 по очкам, целиком без страниц.
    Points,
    /// Модераторы, целиком без страниц, с ролями.
    Moderators,
    /// Топ посещаемости. `days` – `login_streak`, число дней подряд.
    DailyStreak { days: Option<u32> },
    /// Топ чтения или просмотра за неделю. На проверенных сайтах пуст.
    WeeklyViews,
}

impl UserFilter {
    fn param(self) -> Option<&'static str> {
        match self {
            UserFilter::All => None,
            UserFilter::WeeklyTop => Some("weekly-top"),
            UserFilter::Points => Some("points"),
            UserFilter::Moderators => Some("moderators"),
            UserFilter::DailyStreak { .. } => Some("daily-streak"),
            UserFilter::WeeklyViews => Some("weekly-views"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserSort {
    /// По дате регистрации (`id`).
    #[default]
    Registered,
    Username,
}

impl UserSort {
    fn param(self) -> &'static str {
        match self {
            UserSort::Registered => "id",
            UserSort::Username => "username",
        }
    }
}

/// Запрос списка пользователей. Без изменений он повторяет страницу сайта: все
/// пользователи, новые первыми.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserQuery {
    pub text: String,
    pub filter: UserFilter,
    pub sort: UserSort,
    pub order: SortOrder,
}

impl UserQuery {
    pub fn search(text: impl Into<String>) -> UserQuery {
        UserQuery {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn to_query(&self, page: u32) -> Vec<(String, String)> {
        let mut p = vec![
            ("page".to_string(), page.max(1).to_string()),
            ("sort_by".into(), self.sort.param().into()),
            ("sort_type".into(), self.order.param().into()),
        ];
        if !self.text.trim().is_empty() {
            p.push(("q".into(), self.text.trim().to_string()));
        }
        if let Some(filter) = self.filter.param() {
            p.push(("filter".into(), filter.into()));
        }
        if let UserFilter::DailyStreak { days: Some(days) } = self.filter {
            p.push(("login_streak".into(), days.to_string()));
        }
        p
    }
}

// ---------------------------------------------------------------------------------------
// Запросы
// ---------------------------------------------------------------------------------------

impl Client {
    /// Список или поиск людей, команд, персонажей, издателей и франшиз. От `Site-Id`
    /// ответ не зависит.
    pub async fn search_entities(&self, kind: EntityKind, query: &EntityQuery, page: u32) -> Result<Page<EntityCard>> {
        let params = query.to_query(kind, page)?;
        self.get_page(Site::MangaLib, kind.path(), &params).await
    }

    /// Карточка одной сущности по `slug_url`. Человека и персонажа сервер находит и
    /// по голому id, команду нет (404).
    pub async fn entity(&self, kind: EntityKind, slug_url: &str) -> Result<EntityCard> {
        self.get_data(Site::MangaLib, &format!("{}/{slug_url}", kind.path()), &())
            .await
    }

    /// Список или поиск пользователей. Для `Points` и `Moderators` сервер отдаёт всех
    /// одной страницей без признака продолжения.
    pub async fn users(&self, query: &UserQuery, page: u32) -> Result<Page<UserCard>> {
        self.get_page(Site::MangaLib, "user", &query.to_query(page)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn has(query: &[(String, String)], key: &str, value: &str) -> bool {
        query.iter().any(|(k, v)| k == key && v == value)
    }

    #[test]
    fn entity_query_params() {
        let q = EntityQuery {
            text: " маг ".into(),
            sort: Some(EntitySort::Subscribers),
            order: SortOrder::Ascending,
            site_ids: vec![1, 5],
            limit: Some(30),
        };
        let p = q.to_query(EntityKind::Character, 2).unwrap();
        for (k, v) in [
            ("page", "2"),
            ("q", "маг"),
            ("sort_by", "subscribes_count"),
            ("sort_type", "asc"),
            ("site_ids[]", "1"),
            ("site_ids[]", "5"),
            ("limit", "30"),
        ] {
            assert!(has(&p, k, v), "{k}={v}");
        }
        // Остальные списки `limit` игнорируют и его не получают.
        let teams = q.to_query(EntityKind::Team, 1).unwrap();
        assert!(teams.iter().all(|(k, _)| k != "limit"));
    }

    #[test]
    fn default_entity_query_sends_page_only() {
        let p = EntityQuery::default().to_query(EntityKind::People, 0).unwrap();
        assert_eq!(p, vec![("page".to_string(), "1".to_string())]);
    }

    #[test]
    fn entity_query_rejects_what_server_rejects() {
        assert!(EntityQuery::search("а").to_query(EntityKind::Team, 1).is_err());
        let likes = EntityQuery {
            sort: Some(EntitySort::Likes),
            ..Default::default()
        };
        assert!(likes.to_query(EntityKind::People, 1).is_err());
        assert!(likes.to_query(EntityKind::Team, 1).is_ok());
        let limit = EntityQuery {
            limit: Some(100),
            ..Default::default()
        };
        assert!(limit.to_query(EntityKind::Character, 1).is_err());
    }

    #[test]
    fn user_query_params() {
        let p = UserQuery::default().to_query(1);
        assert!(has(&p, "sort_by", "id") && has(&p, "sort_type", "desc"));
        assert!(p.iter().all(|(k, _)| k != "filter"));
        let q = UserQuery {
            text: "user1".into(),
            filter: UserFilter::DailyStreak { days: Some(30) },
            sort: UserSort::Username,
            order: SortOrder::Ascending,
        };
        let p = q.to_query(2);
        for (k, v) in [
            ("q", "user1"),
            ("filter", "daily-streak"),
            ("login_streak", "30"),
            ("sort_by", "username"),
            ("sort_type", "asc"),
            ("page", "2"),
        ] {
            assert!(has(&p, k, v), "{k}={v}");
        }
    }

    #[test]
    fn card_of_a_person() {
        let card: EntityCard = serde_json::from_value(json!({
            "id": 113727, "slug": "junji-itou", "slug_url": "113727--junji-itou", "model": "people",
            "name": "Junji Itou", "rus_name": "Дзюндзи Ито", "alt_name": "Ито Дзюндзи\nJunji Ito / 伊藤潤二",
            "cover": {"thumbnail": "t", "default": "d", "md": "m"},
            "subscription": {"is_subscribed": false, "source_type": "people", "source_id": 113727, "relation": null},
            "confirmed": false, "user_id": 1,
            "titles_count_details": {"1": 48, "3": 0, "2": "1", "4": 0, "5": 3},
            "stats": [
                {"value": 52, "formated": "52", "short": "52", "label": "Тайтла", "tag": "titles"},
                {"value": 38666, "formated": "38 666", "short": "38.7 K", "label": "Подписчиков", "tag": "subscribes"}
            ]
        }))
        .unwrap();
        assert_eq!(card.display_name(), "Дзюндзи Ито");
        assert_eq!(card.alt_names(), vec!["Ито Дзюндзи", "Junji Ito", "伊藤潤二"]);
        assert_eq!(card.titles_count(), Some(52));
        assert_eq!(card.subscribers(), Some(38666));
        assert_eq!(card.titles_on(1), 48);
        assert_eq!(card.titles_on(2), 1);
        assert_eq!(card.sites_with_titles(), vec![1, 2, 5]);
        assert!(card.description().is_empty());
    }

    #[test]
    fn card_survives_odd_shapes() {
        // Пустые объекты PHP приходят списками, имя издателя бывает без русского.
        let card: EntityCard = serde_json::from_value(json!({
            "id": "6848", "slug": "", "slug_url": "6848--", "model": "publisher",
            "name": "페이즈", "rus_name": null, "cover": null, "subscription": null,
            "titles_count_details": [], "stats": [{"broken": true}]
        }))
        .unwrap();
        assert_eq!(card.id, 6848);
        assert_eq!(card.display_name(), "페이즈");
        assert!(card.titles_count_details.is_empty());
        assert!(card.alt_names().is_empty());
    }

    #[test]
    fn character_description_is_a_document() {
        let card: EntityCard = serde_json::from_value(json!({
            "id": 1, "slug_url": "1--x", "model": "character", "name": "X",
            "dsc": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Главная героиня"}]}]}
        }))
        .unwrap();
        assert_eq!(card.description().plain_text(), "Главная героиня");
    }

    #[test]
    fn user_card_variants() {
        let user: UserCard = serde_json::from_value(json!({
            "id": 4064, "username": "user1",
            "avatar": {"filename": null, "url": "https://cover.cdnlibs.org/static/images/placeholders/user_avatar.png"},
            "created_at": "2017-07-29T16:02:09.000000Z", "can_view_profile": true,
            "login_streak": {"last_login_at": null}, "premium": {"enabled": true},
            "points_info": {"top": 1, "total_points": 2657023, "level": 232, "max_level_points": 22968,
                            "current_level_points": 4219, "point_percent_progress": 18.4},
            "roles": [{"id": 11, "name": "moderated", "rus_name": "Модератор"}]
        }))
        .unwrap();
        assert!(user.is_premium());
        assert!(user.avatar.as_ref().unwrap().is_placeholder());
        assert_eq!(user.points_info.as_ref().unwrap().top, Some(1));
        assert_eq!(user.roles[0].rus_name, "Модератор");
    }
}
