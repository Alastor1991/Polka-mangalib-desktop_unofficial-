//! Карточки: коллекционная игра внутри сайтов сети. Карту рисует сообщество, игрок
//! получает экземпляры за чтение и активность, копит осколки по тайтлам, собирает
//! альбомы и меняется картами. Здесь чтение всех разделов и действия, кроме платных.
//!
//! Что надо знать:
//!
//! - ответы почти не зависят от `Site-Id`, поэтому везде идёт `Site::MangaLib`. Исключение
//!   одно: сервер прячет карты, которых нет на сайте запроса, и считает их в
//!   `hidden_site_cards_count`;
//! - `type` тайтла в путях и параметрах равен `manga` для манги, ранобэ и слэша и `anime`
//!   для аниме (`TitleKind`). Слово `book` в поле `model` ответов ничего не значит;
//! - id элемента инвентаря – это `item_user_id`, именно его принимают блокировка, папки,
//!   обмены и хаос-обмен, а не id карты. `locked` значит «занят в лоте или обмене»,
//!   `user_locked` – ручная блокировка владельцем;
//! - баланс осколков хранится в десятых долях (`amount_units`), и считается он по
//!   тайтлу. Ранг S за осколки не выдаётся, `exchange_options` содержит ранги E–A;
//! - у торговой площадки пагинация лежит в `data.pagination`, а не в `meta`; значения
//!   `all` в параметры слать нельзя (422), поэтому «все» у нас просто не отправляется;
//! - запросы на запись написаны по скриптам сайта и вживую не проверены. Их ответы разбираются
//!   терпимо: действие уже выполнено, а разбор ответа не должен превращать успех в ошибку.
//!   Платежи, открытие паков и всё, что тратит деньги, здесь намеренно не реализовано.
//!   Награду альбома (`claim_album_reward`) вызывайте только при `can_claim`: чем платится
//!   `claim_cost`, из ответов не ясно;
//! - карточка без входа читается (каталог, карта, владельцы, чужой открытый инвентарь, альбомы,
//!   рынок), осколки, хаос-обмен и свои обмены требуют входа и без него отвечают 401.

use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::client::{Auth, Client, join_url};
use crate::error::{Error, Result};
use crate::listing::Page;
use crate::models::TitleShort;
use crate::serde_util::{f32_lenient, lenient_vec, null_default, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;

// ------------------------------------------------------------------ перечисления

/// Ранг карты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Rank {
    /// Обычная.
    E,
    /// Необычная.
    D,
    /// Редкая.
    C,
    /// Эпическая.
    B,
    /// Легендарная.
    A,
    /// Мифическая: за осколки не выдаётся.
    S,
}

impl Rank {
    pub const ALL: [Rank; 6] = [Rank::E, Rank::D, Rank::C, Rank::B, Rank::A, Rank::S];

    pub fn code(self) -> &'static str {
        match self {
            Rank::E => "E",
            Rank::D => "D",
            Rank::C => "C",
            Rank::B => "B",
            Rank::A => "A",
            Rank::S => "S",
        }
    }

    pub fn from_code(code: &str) -> Option<Rank> {
        Rank::ALL
            .into_iter()
            .find(|r| r.code().eq_ignore_ascii_case(code.trim()))
    }

    pub fn label(self) -> &'static str {
        match self {
            Rank::E => "Обычная",
            Rank::D => "Необычная",
            Rank::C => "Редкая",
            Rank::B => "Эпическая",
            Rank::A => "Легендарная",
            Rank::S => "Мифическая",
        }
    }
}

/// Тип тайтла в путях карточек: манга, ранобэ и слэш – `manga`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TitleKind {
    Manga,
    Anime,
}

impl TitleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TitleKind::Manga => "manga",
            TitleKind::Anime => "anime",
        }
    }

    pub fn of_site(site: Site) -> TitleKind {
        if site.is_anime() {
            TitleKind::Anime
        } else {
            TitleKind::Manga
        }
    }
}

/// Версия карты: обычная или анимированная.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardKind {
    Normal,
    Animated,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CardKind::Normal => "normal",
            CardKind::Animated => "animated",
        }
    }
}

/// Порядок каталога карт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSort {
    Name,
    CreatedAt,
    Subscribes,
    Rank,
}

impl CardSort {
    fn param(self) -> &'static str {
        match self {
            CardSort::Name => "name",
            CardSort::CreatedAt => "created_at",
            CardSort::Subscribes => "subscribes_count",
            CardSort::Rank => "rank",
        }
    }
}

/// Порядок инвентаря.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventorySort {
    Name,
    CreatedAt,
    Rank,
    PowerScore,
}

impl InventorySort {
    fn param(self) -> &'static str {
        match self {
            InventorySort::Name => "name",
            InventorySort::CreatedAt => "created_at",
            InventorySort::Rank => "rank",
            InventorySort::PowerScore => "power_score",
        }
    }
}

fn push(params: &mut Vec<(String, String)>, key: &str, value: impl ToString) {
    params.push((key.to_string(), value.to_string()));
}

/// Поиск сервер принимает от двух символов.
fn push_text(params: &mut Vec<(String, String)>, text: &str) {
    let text = text.trim();
    if text.chars().count() >= 2 {
        push(params, "q", text);
    }
}

// --------------------------------------------------------------------- страницы

/// Пагинация торговой площадки и слотов альбома.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pagination {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub page: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub current_page: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub last_page: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub per_page: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub has_next_page: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub has_prev_page: bool,
}

// ----------------------------------------------------------------------- карта

/// Картинка карты. `default` – полный адрес на `cover.cdnlibs.org`, `sm` – превью.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardImage {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub default: String,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub sm: Option<String>,
}

/// Ранг карты с рамкой. Пути рамок относительные, от домена сайта.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardRankInfo {
    #[serde(default, deserialize_with = "string_lenient")]
    pub id: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub value: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    #[serde(default)]
    pub frame: Value,
}

impl CardRankInfo {
    pub fn rank(&self) -> Option<Rank> {
        Rank::from_code(&self.id).or_else(|| Rank::from_code(&self.value))
    }
}

/// Счётчик под картой: «Тайтлов», «Подписчиков».
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardCounter {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub tag: String,
}

/// Публичная карточка пользователя в ответах карточек.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardUser {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<UserAvatar>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserAvatar {
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub url: String,
}

/// Владелец карты.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardOwner {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<UserAvatar>,
    /// Сколько экземпляров этой карты у него.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub copies_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardOwners {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub preview: Vec<CardOwner>,
}

/// Карта-шаблон. В списке заполнена основная часть, `media`, `albums`, `owners`,
/// `versions` и подобное есть только в подробной карте (`Client::card`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    /// Автор карты.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub user_id: Option<u64>,
    /// Сайты, где карта видна.
    #[serde(default, deserialize_with = "null_default")]
    pub site_id: Vec<u8>,
    #[serde(default, deserialize_with = "null_default")]
    pub site_restricted: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub cover: CardImage,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub description: String,
    #[serde(default, deserialize_with = "null_default")]
    pub rank: CardRankInfo,
    #[serde(default)]
    pub rank_reason: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub frame_set_id: Option<u64>,
    /// Рамка: путь от домена сайта, для загрузки нужен `Referer`.
    #[serde(default)]
    pub frame_url: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    /// `normal` или `animated`.
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub type_label: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub base_card_id: Option<u64>,
    #[serde(default)]
    pub base_element: Option<String>,
    #[serde(default)]
    pub base_element_label: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub droppable: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub hidden: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub exchangeable: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub stats: Vec<CardCounter>,
    #[serde(default, deserialize_with = "null_default")]
    pub has_instances: bool,
    /// Тайтлы, к которым привязана карта. Только в подробной карте и у экземпляров.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub media: Vec<TitleShort>,
    #[serde(default, deserialize_with = "null_default")]
    pub characters: Vec<Value>,
    /// Автор карты целиком. Только в подробной карте.
    #[serde(default)]
    pub user: Option<Value>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub albums: Vec<AlbumEntry>,
    #[serde(default)]
    pub owners: Option<CardOwners>,
    #[serde(default, deserialize_with = "null_default")]
    pub has_active_market_lots: bool,
    /// Архивные и актуальная версии карты.
    #[serde(default, deserialize_with = "null_default")]
    pub versions: Vec<Value>,
}

impl Card {
    pub fn is_animated(&self) -> bool {
        self.kind == "animated"
    }

    pub fn rank(&self) -> Option<Rank> {
        self.rank.rank()
    }
}

/// Параметры каталога карт.
#[derive(Debug, Clone, Default)]
pub struct CardQuery {
    pub text: String,
    pub page: u32,
    /// Размер страницы. Сайт просит 18, 20 или 28.
    pub limit: Option<u32>,
    pub sort: Option<CardSort>,
    pub descending: bool,
    pub rank: Option<Rank>,
    /// Только карты, видимые на этом сайте сети.
    pub site: Option<Site>,
    /// Номер сайта вместо `site`, например 3 для RanobeLib, которого в `Site` нет.
    pub site_id: Option<u8>,
    pub animated_only: bool,
    /// Карты одного тайтла.
    pub media: Option<(TitleKind, u64)>,
    /// Каталог для выбора карты в лот.
    pub trade_catalog: bool,
}

impl CardQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "page", self.page.max(1));
        if let Some(limit) = self.limit {
            push(&mut p, "limit", limit);
        }
        push_text(&mut p, &self.text);
        if let Some(sort) = self.sort {
            push(&mut p, "sort_by", sort.param());
            push(&mut p, "sort_type", if self.descending { "desc" } else { "asc" });
        }
        if let Some(rank) = self.rank {
            push(&mut p, "rank", rank.code());
        }
        if let Some(id) = self.site_id.or(self.site.map(|s| s.id())) {
            push(&mut p, "site_id", id);
        }
        if self.animated_only {
            push(&mut p, "animated", 1);
        }
        if let Some((kind, id)) = self.media {
            push(&mut p, "media_id", id);
            push(&mut p, "media_type", kind.as_str());
        }
        if self.trade_catalog {
            push(&mut p, "trade_catalog", 1);
        }
        p
    }
}

// ------------------------------------------------------------------- справочник

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdLabel {
    #[serde(default, alias = "value", deserialize_with = "string_lenient")]
    pub id: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
}

/// Справочник карточек из `/constants`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardConstants {
    #[serde(default, rename = "cardRanks", deserialize_with = "lenient_vec")]
    pub ranks: Vec<CardRankInfo>,
    /// Стихии: `id` равен значению (`ice`), `label` – подпись.
    #[serde(default, rename = "cardElements", deserialize_with = "lenient_vec")]
    pub elements: Vec<IdLabel>,
    #[serde(default, rename = "cardTypes", deserialize_with = "lenient_vec")]
    pub types: Vec<IdLabel>,
    /// Наборы рамок и их файлы, если запрошены.
    #[serde(default, rename = "cardFrameSets", deserialize_with = "null_default")]
    pub frame_sets: Vec<Value>,
    #[serde(default, rename = "cardFrameAssets", deserialize_with = "null_default")]
    pub frame_assets: Vec<Value>,
    /// Хаос-обмен может быть выключен на сервере, хотя ответ на него приходит.
    #[serde(default, rename = "cardChaosExchangeEnabled", deserialize_with = "null_default")]
    pub chaos_exchange_enabled: bool,
}

// -------------------------------------------------------------------- инвентарь

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValueLabel {
    #[serde(default, deserialize_with = "string_lenient")]
    pub value: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
}

/// Характеристики экземпляра. У старых карт их может не быть.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardStats {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub item_user_id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub card_id: u64,
    #[serde(default)]
    pub element: Option<ValueLabel>,
    #[serde(default)]
    pub role: Option<ValueLabel>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub quality: u64,
    #[serde(default)]
    pub quality_tier: Option<ValueLabel>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub hp: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub attack: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub defense: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub speed: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub power_score: u64,
    #[serde(default)]
    pub frame_url: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    /// Пусто, пока игрок не посмотрел выпавшую карту.
    #[serde(default)]
    pub first_previewed_at: Option<String>,
    #[serde(default)]
    pub meta: Value,
}

/// Экземпляр карты в руках игрока.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItem {
    /// `item_user_id`: его принимают блокировка, папки, обмены.
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub order: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub exchangeable: bool,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub folder_id: Option<u64>,
    /// Занят в лоте или обмене.
    #[serde(default, deserialize_with = "null_default")]
    pub locked: bool,
    /// Заблокирован владельцем вручную.
    #[serde(default, deserialize_with = "null_default")]
    pub user_locked: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub can_manage_user_lock: bool,
    #[serde(default)]
    pub item: Option<Card>,
    #[serde(default)]
    pub card_stats: Option<CardStats>,
}

#[derive(Debug, Clone, Default)]
pub struct InventoryQuery {
    pub text: String,
    pub page: u32,
    pub limit: Option<u32>,
    pub sort: Option<InventorySort>,
    pub descending: bool,
    pub rank: Option<Rank>,
    pub kind: Option<CardKind>,
    /// Значение стихии: `ice`, `fire` и так далее.
    pub element: Option<String>,
    pub site_id: Option<u8>,
    pub folder_id: Option<u64>,
    /// Только свободные: не занятые в лотах и обменах.
    pub only_free: bool,
    /// Фильтр по ручной блокировке.
    pub user_locked: Option<bool>,
    pub only_duplicates: bool,
    /// Свободные выше.
    pub locked_last: bool,
}

impl InventoryQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "page", self.page.max(1));
        if let Some(limit) = self.limit {
            push(&mut p, "limit", limit);
        }
        push_text(&mut p, &self.text);
        if let Some(sort) = self.sort {
            push(&mut p, "sort_by", sort.param());
            push(&mut p, "sort_type", if self.descending { "desc" } else { "asc" });
        }
        if let Some(rank) = self.rank {
            push(&mut p, "rank", rank.code());
        }
        if let Some(kind) = self.kind {
            push(&mut p, "type", kind.as_str());
        }
        if let Some(element) = self.element.as_deref().filter(|e| !e.is_empty()) {
            push(&mut p, "element", element);
        }
        if let Some(id) = self.site_id {
            push(&mut p, "site_id", id);
        }
        if let Some(id) = self.folder_id {
            push(&mut p, "folder_id", id);
        }
        if self.only_free {
            push(&mut p, "locked", 0);
        }
        if let Some(v) = self.user_locked {
            push(&mut p, "user_locked", u8::from(v));
        }
        if self.only_duplicates {
            push(&mut p, "only_duplicates", 1);
        }
        if self.locked_last {
            push(&mut p, "locked_last", "true");
        }
        p
    }
}

/// Страница инвентаря.
#[derive(Debug, Clone)]
pub struct InventoryPage {
    pub page: Page<InventoryItem>,
    /// Сколько карт скрыто из-за ограничений сайта.
    pub hidden_site_cards_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardFolder {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub public: bool,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub count: u64,
}

/// Папки инвентаря и права на них.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardFolders {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub folders: Vec<CardFolder>,
    /// Всего карт у пользователя.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub is_owner: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub can_view_private: bool,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub count: u64,
    /// Сколько папок можно завести.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub limit: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub can_create: bool,
}

/// Сколько карт у пользователя по рангам. Приходит без обёртки `data`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CardStatistics {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub user_id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub total_by_ranks: std::collections::BTreeMap<String, u64>,
}

impl CardStatistics {
    pub fn count(&self, rank: Rank) -> u64 {
        self.total_by_ranks.get(rank.code()).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------- альбомы

/// Баланс осколков по тайтлу или альбому. `amount_units` в десять раз больше `amount`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShardBalance {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub amount_units: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub amount: f32,
}

/// Сам альбом: набор карт тайтла или франшизы.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Album {
    #[serde(default, alias = "id", deserialize_with = "u64_lenient")]
    pub album_id: u64,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub franchise_id: Option<u64>,
    #[serde(default)]
    pub franchise_name: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    /// `manga`, `anime`, `ranobe`.
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    #[serde(default, deserialize_with = "null_default")]
    pub is_auto_generated: bool,
    #[serde(default)]
    pub cover: Value,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub cover_site: Option<u64>,
}

/// Альбом в списке и в карте: сам альбом и прогресс по нему.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumEntry {
    pub album: Album,
    #[serde(default)]
    pub franchise: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub source_titles: Vec<Value>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub cards_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub sources_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub likes_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub is_liked_by_me: bool,
    /// Прогресс игрока. Без входа `null`, формы вошедшего вживую не видели.
    #[serde(default)]
    pub progress: Option<Value>,
    #[serde(default)]
    pub shard_balance: Option<ShardBalance>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub duplicates_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub has_duplicates: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub can_claim: bool,
    /// Цена награды. Чем она платится, неизвестно.
    #[serde(default)]
    pub claim_cost: Option<Value>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub missing_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub is_completed: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub badges: Vec<Value>,
    #[serde(default)]
    pub actions: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumSort {
    UpdatedAt,
    Name,
    CardsCount,
    /// Только у списка франшиз.
    Popular,
    /// Только у списка франшиз.
    AlbumsCount,
}

impl AlbumSort {
    fn param(self) -> &'static str {
        match self {
            AlbumSort::UpdatedAt => "updated_at",
            AlbumSort::Name => "name",
            AlbumSort::CardsCount => "cards_count",
            AlbumSort::Popular => "popular",
            AlbumSort::AlbumsCount => "albums_count",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AlbumQuery {
    pub page: u32,
    pub site_id: Option<u8>,
    pub sort: Option<AlbumSort>,
    pub descending: bool,
    pub text: String,
    /// `manga`, `anime`, `ranobe`.
    pub kind: Option<String>,
    pub status: Option<String>,
    pub liked_by_me: bool,
    pub user_status: Option<String>,
}

impl AlbumQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "page", self.page.max(1));
        if let Some(id) = self.site_id {
            push(&mut p, "site_id", id);
        }
        if let Some(sort) = self.sort {
            push(&mut p, "sort", sort.param());
            push(&mut p, "direction", if self.descending { "desc" } else { "asc" });
        }
        push_text(&mut p, &self.text);
        for (key, value) in [
            ("type", &self.kind),
            ("status", &self.status),
            ("user_status", &self.user_status),
        ] {
            if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                push(&mut p, key, v);
            }
        }
        if self.liked_by_me {
            push(&mut p, "liked_by_me", 1);
        }
        p
    }
}

/// Какие слоты альбома показывать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    NotCollected,
    CollectedBefore,
    OwnedNow,
}

impl SlotStatus {
    fn param(self) -> &'static str {
        match self {
            SlotStatus::NotCollected => "not_collected",
            SlotStatus::CollectedBefore => "collected_before",
            SlotStatus::OwnedNow => "owned_now",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotSort {
    LocalNumber,
    Rank,
    Name,
    Owned,
}

impl SlotSort {
    fn param(self) -> &'static str {
        match self {
            SlotSort::LocalNumber => "local_number",
            SlotSort::Rank => "rank",
            SlotSort::Name => "name",
            SlotSort::Owned => "owned",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SlotsQuery {
    pub page: u32,
    pub per_page: Option<u32>,
    pub status: Option<SlotStatus>,
    pub rank: Option<Rank>,
    pub version: Option<CardKind>,
    pub only_duplicates: bool,
    pub sort: Option<SlotSort>,
    pub descending: bool,
}

impl SlotsQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "page", self.page.max(1));
        if let Some(n) = self.per_page {
            push(&mut p, "per_page", n);
        }
        if let Some(s) = self.status {
            push(&mut p, "status", s.param());
        }
        if let Some(r) = self.rank {
            push(&mut p, "rank", r.code());
        }
        if let Some(v) = self.version {
            push(&mut p, "version", v.as_str());
        }
        if self.only_duplicates {
            push(&mut p, "only_duplicates", 1);
        }
        if let Some(s) = self.sort {
            push(&mut p, "sort", s.param());
            push(&mut p, "direction", if self.descending { "desc" } else { "asc" });
        }
        p
    }
}

/// Слот альбома: одно место под карту с её версиями.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumSlot {
    #[serde(deserialize_with = "u64_lenient")]
    pub slot_id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub card_id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub local_number: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub global_number: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub card_name: String,
    /// Код ранга строкой: `B`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub rank: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub rank_label: String,
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub base_card_id: Option<u64>,
    #[serde(default)]
    pub element: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub image: CardImage,
    #[serde(default)]
    pub frame_url: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_spoiler: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub spoiler_hidden: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub site_restricted_hidden: bool,
    /// `not_collected`, `collected_before`, `owned_now`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status_label: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub owned_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub owned_now: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub collected_before: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub versions: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub cards: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub source_titles: Vec<Value>,
    #[serde(default)]
    pub card: Option<Card>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlotPage {
    #[serde(default, rename = "data", deserialize_with = "lenient_vec")]
    pub items: Vec<AlbumSlot>,
    #[serde(default)]
    pub pagination: Pagination,
}

/// Альбом с картами. Поля альбома лежат в ответе рядом со слотами.
#[derive(Debug, Clone, Deserialize)]
pub struct AlbumDetails {
    #[serde(flatten)]
    pub entry: AlbumEntry,
    #[serde(default)]
    pub slots: SlotPage,
    #[serde(default)]
    pub empty_state: Option<Value>,
    /// Доступные фильтры, сортировки и режимы показа.
    #[serde(default)]
    pub meta: Value,
}

/// Альбом пользователя с его прогрессом (`/user/{id}/card-albums`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserAlbum {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub album_id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub type_label: String,
    /// `not_started`, `in_progress`, `completed`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status_label: String,
    #[serde(default)]
    pub cover: Value,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub cover_site: Option<u64>,
    #[serde(default)]
    pub franchise: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub sources: Vec<Value>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total_cards_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub collected_cards_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub remaining_cards_count: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub progress_percent: f32,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub shards_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub duplicates_count: u64,
    #[serde(default)]
    pub shard_balance: Option<ShardBalance>,
    #[serde(default, deserialize_with = "null_default")]
    pub can_claim: bool,
    #[serde(default)]
    pub claim_cost: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_completed: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub is_almost_completed: bool,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub last_card_received_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAlbumSort {
    Updated,
    Progress,
    Name,
}

impl UserAlbumSort {
    fn param(self) -> &'static str {
        match self {
            UserAlbumSort::Updated => "updated",
            UserAlbumSort::Progress => "progress",
            UserAlbumSort::Name => "name",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UserAlbumsQuery {
    pub page: u32,
    pub per_page: Option<u32>,
    pub site_id: Option<u8>,
    pub sort: Option<UserAlbumSort>,
    pub descending: bool,
    /// `in_progress` или `completed`.
    pub status: Option<String>,
}

impl UserAlbumsQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "page", self.page.max(1));
        if let Some(n) = self.per_page {
            push(&mut p, "per_page", n);
        }
        if let Some(id) = self.site_id {
            push(&mut p, "site_id", id);
        }
        if let Some(s) = self.sort {
            push(&mut p, "sort_by", s.param());
            push(&mut p, "sort_type", if self.descending { "desc" } else { "asc" });
        }
        if let Some(s) = self.status.as_deref().filter(|s| !s.is_empty()) {
            push(&mut p, "status", s);
        }
        p
    }
}

/// Сводка по альбомам пользователя: прогресс по сайтам и «что продолжить».
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AlbumsOverview {
    /// Ключи: `all` и номера сайтов, в значении `cards_collected`, `cards_total`,
    /// `progress_percent`, `versions`, `site_options`.
    #[serde(default)]
    pub summary: Value,
    /// Ключи те же, в значении списки альбомов (как `UserAlbum`).
    #[serde(default, rename = "continue")]
    pub continue_albums: Value,
}

// ------------------------------------------------------- коллекция и осколки

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionSummary {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub collected: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub progress_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionCard {
    pub card: Card,
    #[serde(default, deserialize_with = "null_default")]
    pub versions: Vec<Value>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub owned_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub collected: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub normal_collected: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub animated_collected: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub has_animated_version: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionRank {
    #[serde(default, deserialize_with = "string_lenient")]
    pub rank: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub rank_label: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub collected: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub progress_percent: f32,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub cards: Vec<CollectionCard>,
}

/// Вкладка «Карточки» на странице тайтла. У тайтла без карт `ranks` пуст.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleCollection {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub album_id: Option<u64>,
    #[serde(default)]
    pub summary: CollectionSummary,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub ranks: Vec<CollectionRank>,
}

/// Тайтл в ответах осколков. Для запросов бери `kind` (поле `type`), а не `model`: у
/// ранобэ там `book`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShardTitle {
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub model: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub site: Option<u64>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub type_label: String,
    /// Путь страницы тайтла на сайте.
    #[serde(default, deserialize_with = "string_lenient")]
    pub url: String,
    #[serde(default)]
    pub cover: Value,
}

impl ShardTitle {
    pub fn title_kind(&self) -> TitleKind {
        if self.kind == "anime" {
            TitleKind::Anime
        } else {
            TitleKind::Manga
        }
    }
}

/// За сколько осколков можно получить карту ранга.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExchangeOption {
    #[serde(default, deserialize_with = "string_lenient")]
    pub rank: String,
    /// В десятых долях осколка.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub cost_units: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub cost: f32,
    #[serde(default, deserialize_with = "null_default")]
    pub can_exchange: bool,
    /// Ранг собран целиком: обмен даст дубликат.
    #[serde(default, deserialize_with = "null_default")]
    pub duplicate: bool,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub missing_cards_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub total_cards_count: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub missing_shards: f32,
}

/// Осколки по одному тайтлу: баланс и цены обмена.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleShards {
    #[serde(default)]
    pub title: ShardTitle,
    #[serde(default)]
    pub album: Option<Album>,
    #[serde(default)]
    pub franchise: Option<Value>,
    #[serde(default)]
    pub balance: ShardBalance,
    #[serde(default, deserialize_with = "null_default")]
    pub albums: Vec<Value>,
    #[serde(default)]
    pub progress: Option<Value>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub exchange_options: Vec<ExchangeOption>,
}

impl TitleShards {
    pub fn option(&self, rank: Rank) -> Option<&ExchangeOption> {
        self.exchange_options.iter().find(|o| o.rank == rank.code())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LimitItem {
    /// `content` (чтение) или `extra` (прочие действия).
    #[serde(default, deserialize_with = "string_lenient")]
    pub key: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub used: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub limit: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub remaining: u64,
    #[serde(default, deserialize_with = "f32_lenient")]
    pub percent: f32,
}

/// Недельный лимит осколков.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeeklyLimits {
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default)]
    pub reset_at: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<LimitItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardSort {
    Name,
    New,
    Amount,
}

impl ShardSort {
    fn param(self) -> &'static str {
        match self {
            ShardSort::Name => "name",
            ShardSort::New => "new",
            ShardSort::Amount => "amount",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShardTitlesQuery {
    pub page: u32,
    pub per_page: Option<u32>,
    pub title_kind: Option<TitleKind>,
    pub sort: Option<ShardSort>,
    pub descending: bool,
    pub rank: Option<Rank>,
    /// Только тайтлы, где осколков хватает на обмен.
    pub exchangeable: bool,
    pub site_id: Option<u8>,
}

impl ShardTitlesQuery {
    fn params(&self, user_id: u64) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "user_id", user_id);
        push(&mut p, "page", self.page.max(1));
        if let Some(n) = self.per_page {
            push(&mut p, "per_page", n);
        }
        if let Some(k) = self.title_kind {
            push(&mut p, "title_type", k.as_str());
        }
        if let Some(s) = self.sort {
            push(&mut p, "sort_by", s.param());
            push(&mut p, "sort_type", if self.descending { "desc" } else { "asc" });
        }
        if let Some(r) = self.rank {
            push(&mut p, "rank", r.code());
        }
        if self.exchangeable {
            push(&mut p, "exchangeable", 1);
        }
        if let Some(id) = self.site_id {
            push(&mut p, "site_id", id);
        }
        p
    }
}

/// Тайтлы, по которым у игрока есть осколки, и недельный лимит.
#[derive(Debug, Clone)]
pub struct ShardTitlesPage {
    pub page: Page<TitleShards>,
    pub weekly_limits: Option<WeeklyLimits>,
}

/// Игрок, собравший полный набор тайтла.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Collector {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<UserAvatar>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

// --------------------------------------------------------------------- рынок

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarketTab {
    /// «Хочу карту».
    #[default]
    Buy,
    /// «Отдаю карту».
    Sell,
    /// Мои лоты.
    My,
    /// Предложения мне.
    Offers,
}

impl MarketTab {
    pub fn as_str(self) -> &'static str {
        match self {
            MarketTab::Buy => "buy",
            MarketTab::Sell => "sell",
            MarketTab::My => "my",
            MarketTab::Offers => "offers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketSort {
    Newest,
    OffersDesc,
    PoolTotalDesc,
    PoolPickDesc,
    OfferedRank,
    OfferedPower,
    OfferedElement,
    WantedRank,
}

impl MarketSort {
    fn param(self) -> &'static str {
        match self {
            MarketSort::Newest => "newest",
            MarketSort::OffersDesc => "offers_desc",
            MarketSort::PoolTotalDesc => "pool_total_desc",
            MarketSort::PoolPickDesc => "pool_pick_desc",
            MarketSort::OfferedRank => "offered_rank",
            MarketSort::OfferedPower => "offered_power",
            MarketSort::OfferedElement => "offered_element",
            MarketSort::WantedRank => "wanted_rank",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketQuery {
    pub tab: MarketTab,
    pub page: u32,
    pub sort: Option<MarketSort>,
    /// Направление сортировки (`sort_direction`): `asc` или `desc`.
    pub sort_direction: Option<String>,
    /// Статус лотов: `active`, `pending`, `accepted`, `completed` и так далее.
    pub status: Option<String>,
    /// Раздел вкладки «Мои»: `active` или `history`.
    pub my_section: Option<String>,
    pub offers_section: Option<String>,
    pub available_for_me: bool,
    pub animated_only: bool,
    pub text: String,
    /// Параметры, которых здесь нет, например `offered` и `wanted` (как они кодируются,
    /// не выяснено) или `match` с `preview=1`.
    pub extra: Vec<(String, String)>,
}

impl MarketQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut p = Vec::new();
        push(&mut p, "tab", self.tab.as_str());
        push(&mut p, "page", self.page.max(1));
        if let Some(s) = self.sort {
            push(&mut p, "sort", s.param());
        }
        for (key, value) in [
            ("sort_direction", &self.sort_direction),
            ("status", &self.status),
            ("my_section", &self.my_section),
            ("offers_section", &self.offers_section),
        ] {
            if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                push(&mut p, key, v);
            }
        }
        if self.available_for_me {
            push(&mut p, "available_for_me", 1);
        }
        if self.animated_only {
            push(&mut p, "animated", 1);
        }
        push_text(&mut p, &self.text);
        p.extend(self.extra.iter().cloned());
        p
    }
}

/// Диапазон силы карт в группе.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowerRange {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub min: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub max: Option<u64>,
}

/// Строка торговой площадки: группа карт или условие.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketItem {
    /// Строка вида `card:129`.
    #[serde(deserialize_with = "string_lenient")]
    pub id: String,
    /// `card_group` или `condition_group`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub mode: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub card_id: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub rank: Option<String>,
    #[serde(default)]
    pub card_type: Option<String>,
    #[serde(default)]
    pub power_range: Option<PowerRange>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub items_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub orders_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub requests_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub lots_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub offers_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub available_for_me_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub is_available_for_me: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub is_own: bool,
    #[serde(default)]
    pub requirement: Option<String>,
    #[serde(default)]
    pub requirement_text: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    /// Карта группы (`Card`), если это группа карт.
    #[serde(default)]
    pub card: Option<Value>,
    #[serde(default)]
    pub representative_order: Option<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub owned_count: u64,
}

/// Страница торговой площадки. Её пагинация лежит в `data.pagination`.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketPage {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<MarketItem>,
    #[serde(default)]
    pub summary: Value,
    /// Допустимые значения фильтров и сортировок.
    #[serde(default)]
    pub filters: Value,
    #[serde(default)]
    pub pagination: Pagination,
    #[serde(default, deserialize_with = "string_lenient")]
    pub tab: String,
    #[serde(default)]
    pub applied_filters: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardMarketTab {
    Offers,
    Wants,
    Mine,
}

impl CardMarketTab {
    fn param(self) -> &'static str {
        match self {
            CardMarketTab::Offers => "offers",
            CardMarketTab::Wants => "wants",
            CardMarketTab::Mine => "mine",
        }
    }
}

/// Торговля одной картой: лоты, заявки и цены.
#[derive(Debug, Clone, Deserialize)]
pub struct CardMarket {
    #[serde(default)]
    pub card: Option<Card>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub items: Vec<MarketItem>,
    /// `active_lots_count`, `buy_requests_count`, `market_power_min` и другие.
    #[serde(default)]
    pub summary: Value,
    #[serde(default)]
    pub filters: Value,
    #[serde(default)]
    pub pagination: Pagination,
    #[serde(default, deserialize_with = "string_lenient")]
    pub tab: String,
}

/// Лот торговой площадки.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketLot {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// `market_listing`.
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status_label: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub author: Option<CardUser>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub offers_count: u64,
    /// Что получит принявший: `required_quantity`, `items`, `preview_cards`, `power_range`.
    #[serde(default)]
    pub receive: Value,
    /// Что надо отдать: `group_id`, `required_quantity`, `requirement`, `requirement_text`.
    #[serde(default)]
    pub give: Value,
    #[serde(default)]
    pub representative_order: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_guest: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub is_own: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub can_accept: bool,
    /// `auth_required`, `own_lot`, `lot_not_active`, `no_matching_items`,
    /// `not_enough_available_items`.
    #[serde(default)]
    pub cannot_accept_reason: Option<String>,
    /// Мои экземпляры, подходящие под условие лота.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub matching_inventory: Vec<InventoryItem>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub matching_inventory_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub available_inventory_count: u64,
}

/// Подробный заказ: лот или прямой обмен.
#[derive(Debug, Clone, Deserialize)]
pub struct TradeOrder {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub user_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub target_user_id: Option<u64>,
    #[serde(default)]
    pub user: Option<CardUser>,
    #[serde(default)]
    pub target_user: Option<CardUser>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub parent_trade_order_id: Option<u64>,
    /// `market_listing` или `direct_exchange`.
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    /// `buy` или `sell`.
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub want_mode: Option<String>,
    #[serde(default)]
    pub give_mode: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub wanted_card_id: Option<u64>,
    #[serde(default)]
    pub wanted_rank: Option<String>,
    #[serde(default)]
    pub wanted_title_type: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub wanted_title_id: Option<u64>,
    #[serde(default)]
    pub wanted_card_type: Option<String>,
    #[serde(default)]
    pub wanted_element: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub wanted_min_power: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub wanted_quantity: Option<u64>,
    #[serde(default)]
    pub required_rank: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub required_quantity: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub min_offer_quantity: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub pool_pick_count: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub pool_total_count: Option<u64>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub offers_count: u64,
    #[serde(default)]
    pub accepted_at: Option<String>,
    #[serde(default)]
    pub declined_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub cancelled_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Предметы заказа: `side` равен `selling_item`, `give_pool` и так далее.
    #[serde(default, deserialize_with = "null_default")]
    pub items: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub groups: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub fulfillments: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub events: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub offers: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSection {
    Active,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Incoming,
    Outgoing,
}

/// Сводка прямых обменов пользователя.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TradeSummary {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub active: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub incoming_active: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub history: u64,
}

// -------------------------------------------------------- хаос-обмен, топ, паки

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChaosRecipe {
    /// `small`, `medium`, `large`, `epic`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub key: String,
    /// Сколько карт отдать: три.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub input_count: u64,
    #[serde(default)]
    pub input_ranks: Value,
    #[serde(default)]
    pub base_output_rank_weights: Value,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub available_cards_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub can_exchange: bool,
}

/// Хаос-обмен: три карты на одну случайную. Только для вошедших.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChaosExchange {
    #[serde(default, deserialize_with = "null_default")]
    pub has_premium: bool,
    #[serde(default)]
    pub premium_bonus: Value,
    #[serde(default)]
    pub animated_bonus: Value,
    #[serde(default)]
    pub card_types: Value,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub recipes: Vec<ChaosRecipe>,
}

#[derive(Debug, Clone, Default)]
pub struct ChaosCardsQuery {
    pub page: u32,
    pub per_page: Option<u32>,
    pub rank: Option<Rank>,
    pub kind: Option<CardKind>,
    pub search: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreatorPeriod {
    /// `current` или `previous`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub key: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub from: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub to: String,
}

/// Место в топе создателей карт.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreatorPlace {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub place: u64,
    #[serde(default)]
    pub delta: Option<i64>,
    #[serde(default)]
    pub user: CardUser,
    /// Сколько карт автора принято за период.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub accepted_count: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub titles_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub cards_preview: Vec<Value>,
    /// Награда места: `place`, `premium_days`, `crystals`.
    #[serde(default)]
    pub reward: Option<Value>,
}

/// Топ авторов принятых карт за месяц.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreatorTop {
    #[serde(default)]
    pub period: CreatorPeriod,
    #[serde(default, rename = "type", deserialize_with = "string_lenient")]
    pub kind: String,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub top: Vec<CreatorPlace>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub top3: Vec<CreatorPlace>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub rewarded: Vec<CreatorPlace>,
    /// Моё место, если вошёл и есть принятые карты.
    #[serde(default)]
    pub me: Option<CreatorPlace>,
    /// `per_card_reward_crystals` и награды по местам.
    #[serde(default)]
    pub rewards: Value,
    #[serde(default, deserialize_with = "null_default")]
    pub can_manage: bool,
}

/// Пак карт. Покупка и открытие здесь не реализованы, это платное действие.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CardPack {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub cards_per_open: u64,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub cards: Vec<Card>,
    #[serde(default, deserialize_with = "null_default")]
    pub hidden: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub buyable: bool,
}

// -------------------------------------------------------- параметры действий

/// Как сформулировано условие лота «хочу карту».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WantMode {
    ExactCard,
    RankInTitle,
    AnyRank,
}

impl WantMode {
    fn param(self) -> &'static str {
        match self {
            WantMode::ExactCard => "exact_card",
            WantMode::RankInTitle => "rank_in_title",
            WantMode::AnyRank => "any_rank",
        }
    }
}

/// Сайт режет комментарий до 400 символов.
const COMMENT_LIMIT: usize = 400;

fn clip_comment(comment: &str) -> String {
    comment.chars().take(COMMENT_LIMIT).collect()
}

fn insert_extra(body: &mut Value, extra: &Map<String, Value>) {
    for (k, v) in extra {
        body[k] = v.clone();
    }
}

/// Лот «хочу карту».
#[derive(Debug, Clone)]
pub struct BuyLot {
    pub want_mode: WantMode,
    /// Для `ExactCard`: id канонической карты.
    pub wanted_canonical_card_id: Option<u64>,
    pub wanted_rank: Option<Rank>,
    pub wanted_title: Option<(TitleKind, u64)>,
    pub wanted_card_type: Option<CardKind>,
    pub wanted_element: Option<String>,
    pub wanted_min_power: Option<u32>,
    pub wanted_quantity: u32,
    /// Сколько карт из пула выбрать тому, кто принимает лот.
    pub pool_pick_count: u32,
    /// Экземпляры, которые я предлагаю на выбор.
    pub pool_item_user_ids: Vec<u64>,
    pub comment: String,
    /// Поля, которых тут нет: `wanted_frame_generation`, `wanted_card_version`.
    pub extra: Map<String, Value>,
}

impl BuyLot {
    fn body(&self) -> Value {
        let mut body = json!({
            "direction": "buy",
            "want_mode": self.want_mode.param(),
            "give_mode": "item_pool",
            "wanted_quantity": self.wanted_quantity.max(1),
            "pool_pick_count": self.pool_pick_count,
            "pool_item_user_ids": self.pool_item_user_ids,
            "comment": clip_comment(&self.comment),
        });
        if let Some(id) = self.wanted_canonical_card_id {
            body["wanted_canonical_card_id"] = id.into();
        }
        if let Some(rank) = self.wanted_rank {
            body["wanted_rank"] = rank.code().into();
        }
        if let Some((kind, id)) = self.wanted_title {
            body["wanted_title_type"] = kind.as_str().into();
            body["wanted_title_id"] = id.into();
        }
        if let Some(kind) = self.wanted_card_type {
            body["wanted_card_type"] = kind.as_str().into();
        }
        if let Some(element) = &self.wanted_element {
            body["wanted_element"] = element.clone().into();
        }
        if let Some(power) = self.wanted_min_power {
            body["wanted_min_power"] = power.into();
        }
        insert_extra(&mut body, &self.extra);
        body
    }
}

/// Что отдаёт лот «отдаю карту».
#[derive(Debug, Clone)]
pub enum SellGive {
    /// Один экземпляр.
    Single(u64),
    /// Пул: принявший выберет `pick_count` из `item_user_ids`.
    Pool { pick_count: u32, item_user_ids: Vec<u64> },
}

/// Лот «отдаю карту».
#[derive(Debug, Clone)]
pub struct SellLot {
    pub give: SellGive,
    /// Какую карту хочу взамен.
    pub required_rank: Option<Rank>,
    pub required_quantity: u32,
    pub min_offer_quantity: u32,
    pub comment: String,
    pub extra: Map<String, Value>,
}

impl SellLot {
    fn body(&self) -> Value {
        let mut body = json!({
            "direction": "sell",
            "want_mode": "open_offers",
            "give_mode": "open_to_offers",
            "required_quantity": self.required_quantity.max(1),
            "min_offer_quantity": self.min_offer_quantity.max(1),
            "comment": clip_comment(&self.comment),
        });
        if let Some(rank) = self.required_rank {
            body["required_rank"] = rank.code().into();
        }
        match &self.give {
            SellGive::Single(id) => body["selling_item_user_id"] = (*id).into(),
            SellGive::Pool {
                pick_count,
                item_user_ids,
            } => {
                body["pool_pick_count"] = (*pick_count).into();
                body["pool_item_user_ids"] = json!(item_user_ids);
            }
        }
        insert_extra(&mut body, &self.extra);
        body
    }
}

/// Прямое предложение обмена одному игроку.
#[derive(Debug, Clone)]
pub struct DirectTrade {
    pub target_user_id: u64,
    pub message: String,
    /// Мои экземпляры (`item_user_id`), которые отдаю.
    pub give: Vec<u64>,
    /// Экземпляры собеседника, которые прошу.
    pub receive: Vec<u64>,
}

impl DirectTrade {
    fn body(&self) -> Value {
        let group = |side: &str, ids: &[u64]| {
            json!({
                "side": side,
                "purpose": "give",
                "trade_type": "card",
                "requirement_type": "exact_items",
                "selection_policy": "all",
                "item_user_ids": ids,
            })
        };
        json!({
            "type": "direct_exchange",
            "target_user_id": self.target_user_id,
            "message": self.message,
            "groups": [group("creator", &self.give), group("counterparty", &self.receive)],
        })
    }
}

/// Предложение к чужому лоту.
#[derive(Debug, Clone)]
pub enum Offer {
    /// К лоту «хочу карту»: отдаю нужный экземпляр и выбираю из пула лота.
    ForBuyLot {
        wanted_item_user_id: u64,
        selected_pool_item_user_ids: Vec<u64>,
    },
    /// К лоту «отдаю карту»: предлагаю свои экземпляры.
    ForSellLot {
        offer_item_user_ids: Vec<u64>,
        selected_pool_item_user_ids: Vec<u64>,
    },
}

impl Offer {
    fn body(&self) -> Value {
        match self {
            Offer::ForBuyLot {
                wanted_item_user_id,
                selected_pool_item_user_ids,
            } => json!({
                "wanted_item_user_id": wanted_item_user_id,
                "selected_pool_item_user_ids": selected_pool_item_user_ids,
            }),
            Offer::ForSellLot {
                offer_item_user_ids,
                selected_pool_item_user_ids,
            } => json!({
                "offer_item_user_ids": offer_item_user_ids,
                "selected_pool_item_user_ids": selected_pool_item_user_ids,
            }),
        }
    }
}

/// Итог обмена осколков на карту. Разбор терпимый: действие уже выполнено.
#[derive(Debug, Clone)]
pub struct ShardExchange {
    /// Полученная карта: случайная недостающая нужного ранга, а при полном наборе дубликат.
    pub card: Option<Card>,
    pub raw: Value,
}

/// Итог хаос-обмена.
#[derive(Debug, Clone)]
pub struct ChaosResult {
    /// Полученная карта (`output_card`) в том виде, в каком её прислал сервер.
    pub output_card: Option<Value>,
    pub raw: Value,
}

/// Итог подтверждения просмотра выпавшей карты.
#[derive(Debug, Clone, Default)]
pub struct ReceivePreview {
    pub owned: bool,
    /// Первый просмотр: сайт играет звук для рангов S, A, B.
    pub first_preview: bool,
    pub rank: Option<String>,
    pub card_instance_stat_id: Option<u64>,
}

// ------------------------------------------------------------------- запросы

impl Client {
    async fn cards_page<T: DeserializeOwned>(&self, path: &str, query: &[(String, String)]) -> Result<Page<T>> {
        self.get_page(Site::MangaLib, path, query).await
    }

    /// Справочник: ранги, стихии, типы. Вход не нужен.
    pub async fn card_constants(&self) -> Result<CardConstants> {
        let fields = [
            "cardTypes",
            "cardRanks",
            "cardElements",
            "cardFrameSets",
            "cardFrameAssets",
            "cardChaosExchangeEnabled",
        ];
        let query: Vec<(&str, &str)> = fields.iter().map(|f| ("fields[]", *f)).collect();
        self.get_data(Site::MangaLib, "constants", &query).await
    }

    /// Каталог карт.
    pub async fn cards(&self, query: &CardQuery) -> Result<Page<Card>> {
        self.cards_page("cards", &query.params()).await
    }

    /// Подробная карта по id или `slug_url` (`980--ximeno`).
    pub async fn card(&self, id_or_slug: &str) -> Result<Card> {
        self.get_data(Site::MangaLib, &format!("cards/{id_or_slug}"), &()).await
    }

    /// Владельцы карты. `available_for_exchange` оставляет тех, у кого экземпляр свободен.
    pub async fn card_owners(
        &self,
        card_id: u64,
        available_for_exchange: bool,
        page: u32,
        limit: Option<u32>,
    ) -> Result<Page<CardOwner>> {
        let mut p = vec![("page".to_string(), page.max(1).to_string())];
        if let Some(limit) = limit {
            push(&mut p, "limit", limit);
        }
        if available_for_exchange {
            push(&mut p, "available_for_exchange", 1);
        }
        self.cards_page(&format!("cards/{card_id}/owners"), &p).await
    }

    /// Инвентарь пользователя. Чужой читается, пока профиль открыт, иначе 403.
    pub async fn inventory(&self, user_id: u64, query: &InventoryQuery) -> Result<InventoryPage> {
        let page: Page<InventoryItem> = self
            .cards_page(&format!("user/{user_id}/inventory"), &query.params())
            .await?;
        let hidden = page
            .meta
            .get("hidden_site_cards_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(InventoryPage {
            page,
            hidden_site_cards_count: hidden,
        })
    }

    /// Папки инвентаря.
    pub async fn card_folders(&self, user_id: u64) -> Result<CardFolders> {
        self.get_data(Site::MangaLib, &format!("user/{user_id}/card-folders"), &())
            .await
    }

    /// Сколько карт у пользователя по рангам.
    pub async fn card_statistics(&self, user_id: u64) -> Result<CardStatistics> {
        self.get_json(Site::MangaLib, &format!("cards/statistics/{user_id}"), &())
            .await
    }

    /// Список альбомов.
    pub async fn card_albums(&self, query: &AlbumQuery) -> Result<Page<AlbumEntry>> {
        self.cards_page("cards/albums", &query.params()).await
    }

    /// Альбом с картами.
    pub async fn card_album(&self, album_id: u64, query: &SlotsQuery) -> Result<AlbumDetails> {
        self.get_data(Site::MangaLib, &format!("cards/albums/{album_id}"), &query.params())
            .await
    }

    /// Франшизы с альбомами. Форма элемента вживую не видна (список был пуст),
    /// поэтому `Value`.
    pub async fn card_franchises(&self, query: &AlbumQuery) -> Result<Page<Value>> {
        self.cards_page("cards/albums/franchises", &query.params()).await
    }

    /// Одна франшиза: `franchise`, `albums_count`, `cards_count`, `progress`,
    /// `shard_balance`, `albums`.
    pub async fn card_franchise(&self, franchise_id: u64) -> Result<Value> {
        self.get_data(Site::MangaLib, &format!("cards/albums/franchises/{franchise_id}"), &())
            .await
    }

    /// Сводка по альбомам пользователя. Без `site_id` приходят все сайты.
    pub async fn user_albums_overview(&self, user_id: u64, site_id: Option<u8>) -> Result<AlbumsOverview> {
        let p: Vec<(String, String)> = site_id
            .map(|id| ("site_id".to_string(), id.to_string()))
            .into_iter()
            .collect();
        self.get_data(Site::MangaLib, &format!("user/{user_id}/card-albums/info"), &p)
            .await
    }

    /// Альбомы пользователя с прогрессом. Фильтры и сортировки в `page.meta["filters"]`.
    pub async fn user_albums(&self, user_id: u64, query: &UserAlbumsQuery) -> Result<Page<UserAlbum>> {
        self.cards_page(&format!("user/{user_id}/card-albums"), &query.params())
            .await
    }

    /// Коллекция игрока по тайтлу. Прогресс приходит только вошедшему.
    pub async fn title_collection(&self, kind: TitleKind, title_id: u64) -> Result<TitleCollection> {
        self.get_data(
            Site::MangaLib,
            &format!("cards/collection/title/{}/{title_id}", kind.as_str()),
            &(),
        )
        .await
    }

    /// Осколки игрока по тайтлу. Нужен вход, без него 401.
    pub async fn title_shards(&self, kind: TitleKind, title_id: u64) -> Result<TitleShards> {
        self.get_data(
            Site::MangaLib,
            &format!("cards/shards/title/{}/{title_id}", kind.as_str()),
            &(),
        )
        .await
    }

    /// Кто собрал полный набор тайтла. Общее число карт в `page.meta["cards_count"]`.
    pub async fn title_collectors(
        &self,
        kind: TitleKind,
        title_id: u64,
        limit: Option<u32>,
    ) -> Result<Page<Collector>> {
        let p: Vec<(String, String)> = limit
            .map(|l| ("limit".to_string(), l.to_string()))
            .into_iter()
            .collect();
        self.cards_page(&format!("cards/collectors/title/{}/{title_id}", kind.as_str()), &p)
            .await
    }

    /// Тайтлы, по которым у пользователя есть осколки, и недельный лимит.
    pub async fn shard_titles(&self, user_id: u64, query: &ShardTitlesQuery) -> Result<ShardTitlesPage> {
        let page: Page<TitleShards> = self.cards_page("profile/cards/shards", &query.params(user_id)).await?;
        let weekly_limits = page
            .meta
            .get("weekly_limits")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        Ok(ShardTitlesPage { page, weekly_limits })
    }

    /// Торговая площадка.
    pub async fn trade_market(&self, query: &MarketQuery) -> Result<MarketPage> {
        self.get_data(Site::MangaLib, "cards/trade-market", &query.params())
            .await
    }

    /// Торговля одной картой.
    pub async fn trade_market_card(
        &self,
        card_id: u64,
        tab: CardMarketTab,
        available_for_me: bool,
        page: u32,
    ) -> Result<CardMarket> {
        let mut p = vec![
            ("tab".to_string(), tab.param().to_string()),
            ("page".to_string(), page.max(1).to_string()),
        ];
        if available_for_me {
            push(&mut p, "available_for_me", 1);
        }
        self.get_data(Site::MangaLib, &format!("cards/trade-market/cards/{card_id}"), &p)
            .await
    }

    /// Лот торговой площадки. Без входа читается, но принять его нельзя.
    pub async fn market_lot(&self, lot_id: u64) -> Result<MarketLot> {
        self.get_data(Site::MangaLib, &format!("cards/trade-market/lots/{lot_id}"), &())
            .await
    }

    /// Заказ целиком: все поля, предметы, события, предложения.
    pub async fn trade_order(&self, order_id: u64) -> Result<TradeOrder> {
        self.get_data(Site::MangaLib, &format!("cards/trade-orders/{order_id}"), &())
            .await
    }

    /// Прямые обмены пользователя. Только свои: чужие дают 403.
    pub async fn direct_trades(
        &self,
        user_id: u64,
        section: OrderSection,
        direction: Option<OrderDirection>,
        page: u32,
        per_page: Option<u32>,
    ) -> Result<Page<TradeOrder>> {
        let mut p = vec![
            ("type".to_string(), "direct_exchange".to_string()),
            ("participant_user_id".to_string(), user_id.to_string()),
            (
                "section".to_string(),
                match section {
                    OrderSection::Active => "active",
                    OrderSection::History => "history",
                }
                .to_string(),
            ),
            ("page".to_string(), page.max(1).to_string()),
        ];
        if let Some(d) = direction {
            push(
                &mut p,
                "direction",
                match d {
                    OrderDirection::Incoming => "incoming",
                    OrderDirection::Outgoing => "outgoing",
                },
            );
        }
        if let Some(n) = per_page {
            push(&mut p, "per_page", n);
        }
        self.cards_page("trade-orders", &p).await
    }

    /// Сколько у пользователя активных и прошлых прямых обменов. Только свои.
    pub async fn direct_trades_summary(&self, user_id: u64) -> Result<TradeSummary> {
        let p = [("participant_user_id", user_id.to_string())];
        self.get_data(Site::MangaLib, "trade-orders/summary", &p).await
    }

    /// Хаос-обмен: рецепты и сколько карт подходит. Нужен вход, и на сервере он бывает
    /// выключен (`CardConstants::chaos_exchange_enabled`).
    pub async fn chaos_exchange(&self) -> Result<ChaosExchange> {
        self.get_data(Site::MangaLib, "cards/chaos-exchange", &()).await
    }

    /// Карты инвентаря, подходящие под рецепт хаос-обмена.
    pub async fn chaos_exchange_cards(&self, recipe: &str, query: &ChaosCardsQuery) -> Result<Page<InventoryItem>> {
        let mut p = vec![
            ("recipe".to_string(), recipe.to_string()),
            ("page".to_string(), query.page.max(1).to_string()),
        ];
        if let Some(n) = query.per_page {
            push(&mut p, "per_page", n);
        }
        if let Some(r) = query.rank {
            push(&mut p, "rank", r.code());
        }
        if let Some(k) = query.kind {
            push(&mut p, "type", k.as_str());
        }
        push_text(&mut p, &query.search);
        // Для этого списка поиск называется `search`, а не `q`.
        for (key, _) in p.iter_mut() {
            if key == "q" {
                *key = "search".to_string();
            }
        }
        self.cards_page("cards/chaos-exchange/cards", &p).await
    }

    /// Топ авторов принятых карт. `period`: `current` или `previous`.
    pub async fn creator_top(&self, kind: Option<CardKind>, period: Option<&str>) -> Result<CreatorTop> {
        let mut p = Vec::new();
        if let Some(k) = kind {
            push(&mut p, "type", k.as_str());
        }
        if let Some(period) = period.filter(|s| !s.is_empty()) {
            push(&mut p, "period", period);
        }
        self.get_data(Site::MangaLib, "cards/creator-top", &p).await
    }

    /// Паки карт. Сейчас на сервере пусто.
    pub async fn card_packs(&self, page: u32) -> Result<Page<CardPack>> {
        let p = vec![("page".to_string(), page.max(1).to_string())];
        self.cards_page("card-packs", &p).await
    }

    pub async fn card_pack(&self, pack_id: u64) -> Result<CardPack> {
        self.get_data(Site::MangaLib, &format!("card-packs/{pack_id}"), &())
            .await
    }

    // ------------------------------------------------------------- действия

    /// Запрос на запись с терпимым разбором: пустое тело и тело без `data` не ошибка.
    async fn card_action<B: Serialize + ?Sized>(&self, method: Method, path: &str, body: Option<&B>) -> Result<Value> {
        let response = self
            .send_api(Auth::User, |http, base| {
                let request = http
                    .request(method.clone(), join_url(base, path))
                    .headers(Self::site_headers(Site::MangaLib));
                match body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await?;
        let bytes = response.bytes().await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: path.to_string(),
            source,
        })?;
        Ok(match value.get("data") {
            Some(data) => data.clone(),
            None => value,
        })
    }

    /// Блокирует экземпляр от обменов и лотов. `item_user_id` – `InventoryItem::id`.
    pub async fn lock_instance(&self, item_user_id: u64) -> Result<()> {
        self.card_action(
            Method::POST,
            &format!("user/inventory/{item_user_id}/lock"),
            None::<&()>,
        )
        .await
        .map(drop)
    }

    pub async fn unlock_instance(&self, item_user_id: u64) -> Result<()> {
        self.card_action(
            Method::DELETE,
            &format!("user/inventory/{item_user_id}/lock"),
            None::<&()>,
        )
        .await
        .map(drop)
    }

    /// Создаёт папку. Ответ – папка, но её форма вживую не проверена, поэтому `Value`.
    pub async fn create_card_folder(&self, name: &str, public: bool) -> Result<Value> {
        let body = json!({"name": name, "public": public});
        self.card_action(Method::POST, "user/card-folders", Some(&body)).await
    }

    pub async fn update_card_folder(
        &self,
        folder_id: u64,
        name: &str,
        color: Option<&str>,
        public: bool,
    ) -> Result<Value> {
        let body = json!({"name": name, "color": color, "public": public});
        self.card_action(Method::PUT, &format!("user/card-folders/{folder_id}"), Some(&body))
            .await
    }

    pub async fn delete_card_folder(&self, folder_id: u64) -> Result<()> {
        self.card_action(Method::DELETE, &format!("user/card-folders/{folder_id}"), None::<&()>)
            .await
            .map(drop)
    }

    /// Порядок папок: все id в нужной последовательности.
    pub async fn reorder_card_folders(&self, folder_ids: &[u64]) -> Result<()> {
        let body = json!({"folder_ids": folder_ids});
        self.card_action(Method::PUT, "user/card-folders/order", Some(&body))
            .await
            .map(drop)
    }

    /// Переносит экземпляры в папку, а с `None` убирает из папок.
    pub async fn move_to_card_folder(&self, item_user_ids: &[u64], folder_id: Option<u64>) -> Result<()> {
        let body = json!({"item_user_ids": item_user_ids, "folder_id": folder_id});
        self.card_action(Method::PUT, "user/card-folders/move", Some(&body))
            .await
            .map(drop)
    }

    /// Меняет осколки тайтла на случайную недостающую карту ранга. Ранг S обменом не
    /// выдаётся. Тратит осколки: вызывайте по явной команде.
    pub async fn exchange_shards(&self, kind: TitleKind, title_id: u64, rank: Rank) -> Result<ShardExchange> {
        let body = json!({"rank": rank.code()});
        let raw = self
            .card_action(
                Method::POST,
                &format!("cards/shards/title/{}/{title_id}/exchange", kind.as_str()),
                Some(&body),
            )
            .await?;
        let card = raw.get("card").and_then(|c| serde_json::from_value(c.clone()).ok());
        Ok(ShardExchange { card, raw })
    }

    /// Забирает награду за собранный альбом. Только свою. Смотрите `can_claim`.
    pub async fn claim_album_reward(&self, user_id: u64, album_id: u64) -> Result<Value> {
        self.card_action(
            Method::POST,
            &format!("user/{user_id}/card-albums/{album_id}/claim"),
            None::<&()>,
        )
        .await
    }

    /// Хаос-обмен: три экземпляра на одну случайную карту. Отданные карты пропадают.
    pub async fn chaos_exchange_run(&self, recipe: &str, item_user_ids: &[u64]) -> Result<ChaosResult> {
        let body = json!({"recipe": recipe, "item_user_ids": item_user_ids});
        let raw = self
            .card_action(Method::POST, "cards/chaos-exchange", Some(&body))
            .await?;
        let output_card = raw.get("output_card").filter(|v| !v.is_null()).cloned();
        Ok(ChaosResult { output_card, raw })
    }

    /// Предлагает обмен игроку напрямую.
    pub async fn create_direct_trade(&self, trade: &DirectTrade) -> Result<Value> {
        self.card_action(Method::POST, "trade-orders", Some(&trade.body()))
            .await
    }

    pub async fn accept_direct_trade(&self, order_id: u64) -> Result<Value> {
        self.card_action(Method::POST, &format!("trade-orders/{order_id}/accept"), None::<&()>)
            .await
    }

    pub async fn decline_direct_trade(&self, order_id: u64) -> Result<Value> {
        self.card_action(Method::POST, &format!("trade-orders/{order_id}/decline"), None::<&()>)
            .await
    }

    pub async fn cancel_direct_trade(&self, order_id: u64) -> Result<Value> {
        self.card_action(Method::POST, &format!("trade-orders/{order_id}/cancel"), None::<&()>)
            .await
    }

    /// Выставляет лот «хочу карту». Предложенные в пуле экземпляры блокируются.
    pub async fn create_buy_lot(&self, lot: &BuyLot) -> Result<Value> {
        self.card_action(Method::POST, "cards/trade-orders", Some(&lot.body()))
            .await
    }

    /// Выставляет лот «отдаю карту».
    pub async fn create_sell_lot(&self, lot: &SellLot) -> Result<Value> {
        self.card_action(Method::POST, "cards/trade-orders", Some(&lot.body()))
            .await
    }

    /// Принимает лот рынка, обмен выполняется сразу. `give` – мои экземпляры,
    /// `receive` – выбранные из пула лота. Ответ: `{lot: {...}}`.
    pub async fn accept_market_lot(&self, lot_id: u64, give: &[u64], receive: &[u64]) -> Result<Value> {
        let body = json!({"item_user_ids": give, "selected_receive_item_user_ids": receive});
        self.card_action(
            Method::POST,
            &format!("cards/trade-market/lots/{lot_id}/accept"),
            Some(&body),
        )
        .await
    }

    /// Предложение к чужому лоту.
    pub async fn offer_to_lot(&self, order_id: u64, offer: &Offer) -> Result<Value> {
        self.card_action(
            Method::POST,
            &format!("cards/trade-orders/{order_id}/offers"),
            Some(&offer.body()),
        )
        .await
    }

    pub async fn accept_offer(&self, offer_id: u64) -> Result<Value> {
        self.card_action(
            Method::POST,
            &format!("cards/trade-offers/{offer_id}/accept"),
            None::<&()>,
        )
        .await
    }

    pub async fn decline_offer(&self, offer_id: u64) -> Result<Value> {
        self.card_action(
            Method::POST,
            &format!("cards/trade-offers/{offer_id}/decline"),
            None::<&()>,
        )
        .await
    }

    /// Снимает свой лот.
    pub async fn cancel_lot(&self, order_id: u64) -> Result<Value> {
        self.card_action(
            Method::POST,
            &format!("cards/trade-orders/{order_id}/cancel"),
            None::<&()>,
        )
        .await
    }

    /// Подтверждает просмотр выпавшей карты (`CardStats::id` экземпляра).
    pub async fn confirm_receive_preview(&self, card_id: u64, card_instance_stat_id: u64) -> Result<ReceivePreview> {
        let body = json!({"card_instance_stat_id": card_instance_stat_id});
        let raw = self
            .card_action(Method::POST, &format!("cards/{card_id}/receive-preview"), Some(&body))
            .await?;
        Ok(ReceivePreview {
            owned: raw.get("owned").and_then(Value::as_bool).unwrap_or(false),
            first_preview: raw.get("first_preview").and_then(Value::as_bool).unwrap_or(false),
            rank: raw.get("rank").and_then(Value::as_str).map(str::to_string),
            card_instance_stat_id: raw.get("card_instance_stat_id").and_then(Value::as_u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(p: &[(String, String)], k: &str, v: &str) -> bool {
        p.iter().any(|(a, b)| a == k && b == v)
    }

    #[test]
    fn card_query_params() {
        let q = CardQuery {
            text: " бе ".into(),
            page: 0,
            sort: Some(CardSort::Subscribes),
            descending: true,
            rank: Some(Rank::A),
            site_id: Some(3),
            animated_only: true,
            media: Some((TitleKind::Manga, 7965)),
            ..Default::default()
        };
        let p = q.params();
        assert!(has(&p, "page", "1"));
        assert!(has(&p, "q", "бе"));
        assert!(has(&p, "sort_by", "subscribes_count") && has(&p, "sort_type", "desc"));
        assert!(has(&p, "rank", "A") && has(&p, "site_id", "3") && has(&p, "animated", "1"));
        assert!(has(&p, "media_id", "7965") && has(&p, "media_type", "manga"));
        // Один символ в поиск не уходит, сортировки без запроса нет.
        let p = CardQuery {
            text: "я".into(),
            ..Default::default()
        }
        .params();
        assert!(!p.iter().any(|(k, _)| k == "q" || k == "sort_by"));
    }

    #[test]
    fn inventory_query_params() {
        let q = InventoryQuery {
            kind: Some(CardKind::Normal),
            element: Some("ice".into()),
            only_free: true,
            user_locked: Some(false),
            only_duplicates: true,
            locked_last: true,
            folder_id: Some(4),
            ..Default::default()
        };
        let p = q.params();
        assert!(has(&p, "type", "normal") && has(&p, "element", "ice") && has(&p, "locked", "0"));
        assert!(has(&p, "user_locked", "0") && has(&p, "only_duplicates", "1"));
        assert!(has(&p, "locked_last", "true") && has(&p, "folder_id", "4"));
    }

    #[test]
    fn lot_bodies() {
        let buy = BuyLot {
            want_mode: WantMode::RankInTitle,
            wanted_canonical_card_id: None,
            wanted_rank: Some(Rank::B),
            wanted_title: Some((TitleKind::Manga, 89959)),
            wanted_card_type: None,
            wanted_element: None,
            wanted_min_power: None,
            wanted_quantity: 0,
            pool_pick_count: 1,
            pool_item_user_ids: vec![5, 6],
            comment: "я".repeat(500),
            extra: Map::new(),
        };
        let body = buy.body();
        assert_eq!(body["wanted_quantity"], 1);
        assert_eq!(body["wanted_title_type"], "manga");
        assert_eq!(body["comment"].as_str().unwrap().chars().count(), 400);
        assert!(body.get("wanted_element").is_none());

        let sell = SellLot {
            give: SellGive::Single(9),
            required_rank: None,
            required_quantity: 1,
            min_offer_quantity: 1,
            comment: String::new(),
            extra: Map::new(),
        };
        let body = sell.body();
        assert_eq!(body["selling_item_user_id"], 9);
        assert_eq!(body["want_mode"], "open_offers");
        assert!(body.get("pool_item_user_ids").is_none());
    }

    #[test]
    fn rank_codes() {
        assert_eq!(Rank::from_code(" s "), Some(Rank::S));
        assert_eq!(Rank::from_code("all"), None);
        assert!(Rank::S > Rank::E);
    }
}
