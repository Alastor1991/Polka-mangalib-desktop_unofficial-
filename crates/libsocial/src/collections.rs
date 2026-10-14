//! Подборки: лента, подборки пользователя, одна подборка с блоками, создание, правка,
//! удаление и голос.
//!
//! Подборка – список тайтлов, персонажей или людей, разбитый на блоки с комментариями
//! автора. Ловушки сервиса:
//! - `limit` в списке только от 12 до 24, иначе 422; клиент подгоняет значение сам;
//! - `sort_by` принимает лишь `newest` и `popular`, остальные названия дают 422;
//! - значение «все» в `type` и подобных параметрах сервер не терпит, его просто не
//!   отправляют;
//! - несуществующая и закрытая подборка отвечают одинаково: `Error::NotFound`;
//! - запросы записи по скриптам сайта, вживую их не проверяли.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::comments::Vote;
use crate::error::{Error, Result};
use crate::listing::{Page, SortOrder, bool_lenient, opt_u8_lenient};
use crate::models::{Cover, TitleShort};
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_u64_lenient, u64_lenient};
use crate::site::Site;
use crate::users::UserRef;

/// Наименьший и наибольший `limit`, который принимает сервер.
pub const LIMIT_RANGE: std::ops::RangeInclusive<u32> = 12..=24;

/// Из чего состоит подборка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Titles,
    Character,
    People,
}

impl CollectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Titles => "titles",
            CollectionKind::Character => "character",
            CollectionKind::People => "people",
        }
    }
}

/// Порядок ленты. Сервер знает только эти два.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CollectionSort {
    #[default]
    Newest,
    Popular,
}

impl CollectionSort {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionSort::Newest => "newest",
            CollectionSort::Popular => "popular",
        }
    }
}

/// Запрос к ленте подборок.
#[derive(Debug, Clone)]
pub struct CollectionsQuery {
    pub page: u32,
    /// Сколько на странице. Подгоняется под `LIMIT_RANGE`.
    pub limit: u32,
    pub sort: Option<CollectionSort>,
    pub order: Option<SortOrder>,
    /// Поиск по названию, от двух символов.
    pub text: String,
    /// Только подборки этого пользователя.
    pub user_id: Option<u64>,
    /// Вместе с `user_id`: `true` – избранные подборки пользователя.
    pub favorites: bool,
    /// Только подборки этих сайтов. Пусто – все.
    pub sites: Vec<Site>,
    pub kind: Option<CollectionKind>,
}

impl Default for CollectionsQuery {
    fn default() -> Self {
        CollectionsQuery {
            page: 1,
            limit: *LIMIT_RANGE.start(),
            sort: None,
            order: None,
            text: String::new(),
            user_id: None,
            favorites: false,
            sites: Vec::new(),
            kind: None,
        }
    }
}

impl CollectionsQuery {
    /// Подборки одного пользователя; `favorites` переключает на избранные.
    pub fn of_user(user_id: u64, favorites: bool) -> Self {
        CollectionsQuery {
            user_id: Some(user_id),
            favorites,
            ..Default::default()
        }
    }

    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![
            (
                "limit",
                self.limit.clamp(*LIMIT_RANGE.start(), *LIMIT_RANGE.end()).to_string(),
            ),
            ("page", self.page.max(1).to_string()),
        ];
        if let Some(sort) = self.sort {
            query.push(("sort_by", sort.as_str().into()));
        }
        if let Some(order) = self.order {
            query.push(("sort_type", order.as_str().into()));
        }
        let text = self.text.trim();
        if text.chars().count() >= 2 {
            query.push(("q", text.to_string()));
        }
        if let Some(user) = self.user_id {
            query.push(("user_id", user.to_string()));
            query.push(("subscriptions", if self.favorites { "1" } else { "0" }.into()));
        }
        for site in &self.sites {
            query.push(("site_ids[]", site.id().to_string()));
        }
        if let Some(kind) = self.kind {
            query.push(("type", kind.as_str().into()));
        }
        query
    }
}

/// Голоса за подборку.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionVotes {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub up: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub down: u64,
    /// Мой голос. Против сервер, судя по сайту, присылает как `0` или как `-1`.
    #[serde(default, deserialize_with = "my_vote", serialize_with = "serialize_vote")]
    pub user: Option<Vote>,
}

impl CollectionVotes {
    pub fn score(&self) -> i64 {
        self.up as i64 - self.down as i64
    }
}

fn serialize_vote<S: serde::Serializer>(v: &Option<Vote>, s: S) -> std::result::Result<S::Ok, S::Error> {
    match v {
        Some(Vote::Up) => s.serialize_i8(1),
        Some(Vote::Down) => s.serialize_i8(0),
        None => s.serialize_none(),
    }
}

fn my_vote<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vote>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(Vote::Up),
            Some(0 | -1) => Some(Vote::Down),
            _ => None,
        },
        Value::String(s) => match s.trim() {
            "1" => Some(Vote::Up),
            "0" | "-1" => Some(Vote::Down),
            _ => None,
        },
        _ => None,
    })
}

/// Подборка в ленте: без блоков и без автора, только `user_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionBrief {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default = "default_kind", rename = "type")]
    pub kind: CollectionKind,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub favorites_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub items_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comments_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: CollectionVotes,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub user_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u8_lenient")]
    pub site_id: Option<u8>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub spoiler: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub interactive: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub adult: bool,
    /// Обложки первых элементов.
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub previews: Vec<Cover>,
}

fn default_kind() -> CollectionKind {
    CollectionKind::Titles
}

impl CollectionBrief {
    pub fn site(&self) -> Option<Site> {
        self.site_id.and_then(Site::from_id)
    }
}

/// Подписка на подборку (избранное).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionSubscription {
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_subscribed: bool,
}

/// Элемент блока. У тайтла `related` разбирается через `CollectionItem::title`, у
/// персонажа и человека набор полей другой, и он остаётся как есть.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionItem {
    /// `manga`, `anime`, `character` или `people`.
    #[serde(default, deserialize_with = "null_default")]
    pub item_type: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub item_id: u64,
    /// Комментарий автора подборки.
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub related: Option<Value>,
}

impl CollectionItem {
    /// Карточка тайтла, если элемент – манга или аниме.
    pub fn title(&self) -> Option<TitleShort> {
        if !matches!(self.item_type.as_str(), "manga" | "anime") {
            return None;
        }
        serde_json::from_value(self.related.clone()?).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionBlock {
    #[serde(default, deserialize_with = "null_default")]
    pub uuid: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub items: Vec<CollectionItem>,
}

/// Подборка целиком.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    #[serde(flatten)]
    pub brief: CollectionBrief,
    /// Описание: документ редактора или `null`. Разобранное – `Collection::description_doc`.
    #[serde(default)]
    pub description: Option<Value>,
    /// Картинки из описания, как есть.
    #[serde(default, deserialize_with = "null_default")]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default)]
    pub subscription: Option<CollectionSubscription>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub blocks: Vec<CollectionBlock>,
}

impl Collection {
    pub fn description_doc(&self) -> Doc {
        self.description.as_ref().map(Doc::from_prosemirror).unwrap_or_default()
    }
}

/// Элемент блока в новой подборке. Порядок в блоке задаёт позиция в списке.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftItem {
    pub item_id: u64,
    /// `manga`, `anime`, `character` или `people`, по `CollectionKind` подборки.
    pub item_type: String,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftBlock {
    /// Клиентский идентификатор блока. `DraftBlock::new` делает его сам.
    pub uuid: String,
    pub name: String,
    pub items: Vec<DraftItem>,
}

impl DraftBlock {
    pub fn new(name: impl Into<String>) -> DraftBlock {
        DraftBlock {
            uuid: new_uuid(),
            name: name.into(),
            items: Vec::new(),
        }
    }
}

/// UUID версии 4: сайт ждёт от клиента собственный идентификатор блока.
fn new_uuid() -> String {
    let mut b = [0u8; 16];
    rand::fill(b.as_mut_slice());
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

/// Подборка для создания или правки. Правка отправляет её целиком, как форма сайта.
#[derive(Debug, Clone)]
pub struct CollectionDraft {
    /// Тип меняется только у пустой подборки.
    pub kind: CollectionKind,
    pub name: String,
    /// Короче двух символов уходит как `null`, как у сайта.
    pub description: Option<Doc>,
    /// Картинки из описания, как их вернула загрузка.
    pub attachments: Vec<Value>,
    pub interactive: bool,
    pub private: bool,
    pub spoiler: bool,
    /// Обязательно для тайтлов с жанрами хентай, этти, юри, яой, сёнен-ай и сёдзе-ай.
    pub adult: bool,
    pub blocks: Vec<DraftBlock>,
}

impl CollectionDraft {
    pub fn new(kind: CollectionKind, name: impl Into<String>) -> CollectionDraft {
        CollectionDraft {
            kind,
            name: name.into(),
            description: None,
            attachments: Vec::new(),
            interactive: false,
            private: false,
            spoiler: false,
            adult: false,
            blocks: Vec::new(),
        }
    }

    /// Тело запроса. Подборка без названия отвергается до запроса.
    fn to_body(&self) -> Result<Value> {
        if self.name.trim().is_empty() {
            return Err(Error::Other("у подборки нет названия".into()));
        }
        let description = match &self.description {
            Some(d) if d.plain_text().trim().chars().count() >= 2 => d.to_prosemirror(),
            _ => Value::Null,
        };
        let blocks: Vec<Value> = self
            .blocks
            .iter()
            .map(|b| {
                let items: Vec<Value> = b
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        json!({"item_id": it.item_id, "item_type": it.item_type, "comment": it.comment, "order": i + 1})
                    })
                    .collect();
                json!({"uuid": b.uuid, "name": b.name, "items": items})
            })
            .collect();
        Ok(json!({
            "type": self.kind.as_str(),
            "name": self.name.trim(),
            "description": description,
            "attachments": self.attachments,
            "interactive": self.interactive,
            "private": self.private,
            "spoiler": self.spoiler,
            "adult": self.adult,
            "blocks": blocks,
        }))
    }
}

/// Ответ на создание и правку: сайт берёт из него только `id`.
#[derive(Deserialize)]
struct Saved {
    #[serde(deserialize_with = "u64_lenient")]
    id: u64,
}

impl Client {
    /// Страница ленты подборок. `limit` подгоняется под диапазон сервера.
    pub async fn collections(&self, q: &CollectionsQuery) -> Result<Page<CollectionBrief>> {
        self.get_page(Site::MangaLib, "collections", &q.to_query()).await
    }

    /// Свои подборки или подборки другого пользователя.
    pub async fn user_collections(&self, user_id: u64, page: u32) -> Result<Page<CollectionBrief>> {
        self.collections(&CollectionsQuery {
            page,
            ..CollectionsQuery::of_user(user_id, false)
        })
        .await
    }

    /// Подборки, на которые подписан пользователь.
    pub async fn favorite_collections(&self, user_id: u64, page: u32) -> Result<Page<CollectionBrief>> {
        self.collections(&CollectionsQuery {
            page,
            ..CollectionsQuery::of_user(user_id, true)
        })
        .await
    }

    /// Подборка с блоками. Закрытая и несуществующая дают `Error::NotFound`.
    pub async fn collection(&self, id: u64) -> Result<Collection> {
        self.get_data(Site::MangaLib, &format!("collections/{id}"), &()).await
    }

    /// Создаёт подборку и возвращает её id.
    pub async fn create_collection(&self, draft: &CollectionDraft) -> Result<u64> {
        let body = draft.to_body()?;
        let saved: Saved = self
            .send_data(Site::MangaLib, Method::POST, "collections", &(), Some(&body))
            .await?;
        Ok(saved.id)
    }

    /// Правит подборку целиком. Править может автор и пользователь с правом редактирования.
    pub async fn update_collection(&self, id: u64, draft: &CollectionDraft) -> Result<u64> {
        let body = draft.to_body()?;
        let saved: Saved = self
            .send_data(
                Site::MangaLib,
                Method::PUT,
                &format!("collections/{id}"),
                &(),
                Some(&body),
            )
            .await?;
        Ok(saved.id)
    }

    pub async fn delete_collection(&self, id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("collections/{id}"), None)
            .await
    }

    /// Голос за подборку. Повторный тот же голос снимает его.
    pub async fn vote_collection(&self, id: u64, vote: Vote) -> Result<()> {
        self.vote("collection", id, vote).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_clamped_and_filters_are_omitted() {
        let q = CollectionsQuery {
            limit: 3,
            ..Default::default()
        }
        .to_query();
        assert_eq!(q[0], ("limit", "12".to_string()));
        let q = CollectionsQuery {
            limit: 100,
            ..CollectionsQuery::of_user(5, true)
        }
        .to_query();
        assert_eq!(q[0], ("limit", "24".to_string()));
        assert!(q.contains(&("subscriptions", "1".into())));
        assert!(!q.iter().any(|(k, _)| *k == "type" || *k == "sort_by"));
    }

    #[test]
    fn uuid_has_v4_shape() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
        assert!("89ab".contains(u.as_bytes()[19] as char));
        assert_ne!(u, new_uuid());
    }

    #[test]
    fn draft_body_numbers_items_and_nulls_short_description() {
        let mut d = CollectionDraft::new(CollectionKind::Titles, " Подборка ");
        d.description = Some(Doc::from_plain("я"));
        let mut block = DraftBlock::new("Блок");
        for id in [7965, 123] {
            block.items.push(DraftItem {
                item_id: id,
                item_type: "manga".into(),
                comment: String::new(),
            });
        }
        d.blocks.push(block);
        let body = d.to_body().unwrap();
        assert_eq!(body["name"], "Подборка");
        assert!(body["description"].is_null());
        assert_eq!(body["blocks"][0]["items"][1]["order"], 2);
        assert!(CollectionDraft::new(CollectionKind::Titles, "  ").to_body().is_err());
    }

    #[test]
    fn my_vote_accepts_both_down_values() {
        for (raw, want) in [
            (json!(1), Some(Vote::Up)),
            (json!(-1), Some(Vote::Down)),
            (json!(0), Some(Vote::Down)),
            (json!(null), None),
        ] {
            let v: CollectionVotes = serde_json::from_value(json!({"up": 1, "down": 0, "user": raw})).unwrap();
            assert_eq!(v.user, want);
        }
    }
}
