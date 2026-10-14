//! Справочник значений для формы каталога: жанры, теги, типы, статусы, возрасты,
//! форматы. Это `GET /constants` с перечнем нужных имён в `fields[]`.
//!
//! Справочник общий для всей сети и от `Site-Id` не зависит, отбор по сайту делает
//! клиент по `site_ids` каждого значения, как и сайт. Ловушки:
//!
//! - у значения без `site_ids` (тип «Неизвестный») сайта нет, в формах его не показывают;
//! - в `site_ids` статусов встречается `0` (общая запись), но это не «все сайты»:
//!   статус «Выпуск прекращён» с нулём всё равно не относится к AnimeLib;
//! - возрастной рейтинг AnimeLib лежит в отдельном списке `ageRestrictionAnime`;
//! - жанры и теги могут быть скрыты для страны (`blocked_for_country`), скрывает их клиент;
//! - форматы 8–10 (для RanobeLib) в справочнике есть, а `/manga` их не принимает;
//! - запрос без `fields[]` возвращает пустой объект, неизвестные имена молча пропускаются.
//!
//! Сайт кеширует ответ на двадцать минут. Крейт состояния не держит, кеш за вызывающим.

use serde::{Deserialize, Serialize};

use crate::catalog::{MAX_FILTERABLE_FORMAT, string_list};
use crate::client::Client;
use crate::error::Result;
use crate::serde_util::{null_default, opt_string_lenient, string_lenient, u64_lenient};
use crate::site::Site;

/// Жанр или тег: у них одна и та же структура. Id тегов сквозные, один и тот же id
/// на разных сайтах не повторяется.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Term {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    /// Синонимы, по ним форма сайта ищет подстрокой.
    #[serde(default, rename = "alt_name", deserialize_with = "string_list")]
    pub alt_names: Vec<String>,
    #[serde(default, rename = "dsc", deserialize_with = "string_lenient")]
    pub description: String,
    /// Метка 18+.
    #[serde(default, deserialize_with = "null_default")]
    pub adult: bool,
    /// Предупреждение: сайт ставит у такого значения значок.
    #[serde(default, deserialize_with = "null_default")]
    pub alert: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub site_ids: Vec<u8>,
    /// Двухбуквенные коды стран, где значение скрыто (`RU`).
    #[serde(default, deserialize_with = "string_list")]
    pub blocked_for_country: Vec<String>,
    #[serde(default, deserialize_with = "string_list")]
    pub allowed_for_country: Vec<String>,
    #[serde(default, deserialize_with = "string_list")]
    pub allowed_for_domain: Vec<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub anilist_id: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub shiki_id: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub mal_id: Option<String>,
    #[serde(default)]
    pub is_media_spoiler: Option<bool>,
    #[serde(default)]
    pub is_general_spoiler: Option<bool>,
}

impl Term {
    pub fn is_for(&self, site: u8) -> bool {
        in_site(&self.site_ids, site)
    }

    /// Скрыто ли значение для страны (`RU`). Регистр не важен.
    pub fn is_blocked_in(&self, country: &str) -> bool {
        self.blocked_for_country.iter().any(|c| c.eq_ignore_ascii_case(country))
    }

    /// Подходит ли значение под строку поиска: подстрока без учёта регистра в названии
    /// или в синонимах.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.name.to_lowercase().contains(&query)
            || self.alt_names.iter().any(|a| a.to_lowercase().contains(&query))
    }
}

/// Значение с подписью: тип, статус, статус перевода, возрастной рейтинг.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    #[serde(default, deserialize_with = "null_default")]
    pub site_ids: Vec<u8>,
}

impl Choice {
    pub fn is_for(&self, site: u8) -> bool {
        in_site(&self.site_ids, site)
    }
}

/// Формат выпуска: 4-кома, сборник, вебтун.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub eng_name: String,
    /// Краткая подпись (`LN`), у части форматов `None`.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub short_name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub site_ids: Vec<u8>,
}

impl Format {
    pub fn is_for(&self, site: u8) -> bool {
        in_site(&self.site_ids, site)
    }

    /// Принимает ли такой формат фильтр каталога. Для 8–10 сервер отвечает 422.
    pub fn is_filterable(&self) -> bool {
        (1..=MAX_FILTERABLE_FORMAT).contains(&self.id)
    }
}

/// Пометка содержимого: «нецензурная брань», «упоминание наркотиков».
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentMark {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub label: String,
    /// Юридическая формулировка, которую показывают рядом с пометкой.
    #[serde(default, deserialize_with = "string_lenient")]
    pub formulation: String,
}

/// Справочник формы каталога.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogConstants {
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub genres: Vec<Term>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub tags: Vec<Term>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub types: Vec<Choice>,
    /// Статус тайтла.
    #[serde(default, rename = "status", deserialize_with = "crate::serde_util::lenient_vec")]
    pub statuses: Vec<Choice>,
    /// Статус перевода. Значения 5–7 относятся к AnimeLib, а форма сайта этот
    /// фильтр для аниме не показывает.
    #[serde(
        default,
        rename = "scanlateStatus",
        deserialize_with = "crate::serde_util::lenient_vec"
    )]
    pub scanlate_statuses: Vec<Choice>,
    #[serde(
        default,
        rename = "ageRestriction",
        deserialize_with = "crate::serde_util::lenient_vec"
    )]
    pub age_ratings: Vec<Choice>,
    /// Возрастной рейтинг AnimeLib (PG, PG-13, R-17…).
    #[serde(
        default,
        rename = "ageRestrictionAnime",
        deserialize_with = "crate::serde_util::lenient_vec"
    )]
    pub age_ratings_anime: Vec<Choice>,
    #[serde(default, rename = "format", deserialize_with = "crate::serde_util::lenient_vec")]
    pub formats: Vec<Format>,
    #[serde(
        default,
        rename = "contentMarking",
        deserialize_with = "crate::serde_util::lenient_vec"
    )]
    pub content_marking: Vec<ContentMark>,
}

/// Значение относится к сайту, только если назван он сам. Нуль в списке сайтом не
/// считается, а пустой список значит, что значение нигде не показывается.
fn in_site(site_ids: &[u8], site: u8) -> bool {
    site_ids.contains(&site)
}

impl CatalogConstants {
    pub fn genres_for(&self, site: u8) -> Vec<&Term> {
        self.genres.iter().filter(|t| t.is_for(site)).collect()
    }

    pub fn tags_for(&self, site: u8) -> Vec<&Term> {
        self.tags.iter().filter(|t| t.is_for(site)).collect()
    }

    pub fn types_for(&self, site: u8) -> Vec<&Choice> {
        self.types.iter().filter(|t| t.is_for(site)).collect()
    }

    pub fn statuses_for(&self, site: u8) -> Vec<&Choice> {
        self.statuses.iter().filter(|t| t.is_for(site)).collect()
    }

    pub fn scanlate_statuses_for(&self, site: u8) -> Vec<&Choice> {
        self.scanlate_statuses.iter().filter(|t| t.is_for(site)).collect()
    }

    /// Возрасты сайта: для AnimeLib из отдельного списка с другими подписями.
    pub fn age_ratings_for(&self, site: u8) -> Vec<&Choice> {
        let list = if site == Site::AnimeLib.id() {
            &self.age_ratings_anime
        } else {
            &self.age_ratings
        };
        list.iter().filter(|t| t.is_for(site)).collect()
    }

    /// Все форматы сайта, в том числе те, что каталог не принимает (`Format::is_filterable`).
    pub fn formats_for(&self, site: u8) -> Vec<&Format> {
        self.formats.iter().filter(|t| t.is_for(site)).collect()
    }

    pub fn genre(&self, id: u64) -> Option<&Term> {
        self.genres.iter().find(|t| t.id == id)
    }

    pub fn tag(&self, id: u64) -> Option<&Term> {
        self.tags.iter().find(|t| t.id == id)
    }
}

/// Имена справочников, которые просит форма каталога.
const FIELDS: &[&str] = &[
    "genres",
    "tags",
    "types",
    "status",
    "scanlateStatus",
    "ageRestriction",
    "ageRestrictionAnime",
    "format",
    "contentMarking",
];

impl Client {
    /// Справочник формы каталога одним запросом. Ответ тяжёлый (около 700 жанров
    /// и тегов с описаниями), поэтому кешировать его стоит надолго.
    pub async fn catalog_constants(&self) -> Result<CatalogConstants> {
        let query: Vec<(&str, &str)> = FIELDS.iter().map(|f| ("fields[]", *f)).collect();
        self.get_data(Site::MangaLib, "constants", &query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> CatalogConstants {
        serde_json::from_value(json!({
            "genres": [
                {"id": 34, "name": "Боевик", "alt_name": ["action"], "dsc": "x", "adult": false,
                 "site_ids": [1, 2, 3, 4, 5], "blocked_for_country": []},
                {"id": 84, "name": "Хентай", "alt_name": [], "dsc": "", "adult": true,
                 "site_ids": [5], "blocked_for_country": ["RU"], "anilist_id": 12, "shiki_id": "1446"}
            ],
            "types": [
                {"id": 0, "label": "Неизвестный", "site_ids": []},
                {"id": 1, "label": "Манга", "site_ids": [1, 2, 4]},
                {"id": 16, "label": "TV Сериал", "site_ids": [5]}
            ],
            "status": [
                {"id": 1, "label": "Онгоинг", "site_ids": [0, 1, 5]},
                {"id": 5, "label": "Выпуск прекращён", "site_ids": [0, 1]}
            ],
            "ageRestriction": [{"id": 3, "label": "16+", "site_ids": [1, 3, 5]}],
            "ageRestrictionAnime": [{"id": 3, "label": "R-17", "site_ids": [1, 3, 5]}],
            "format": [
                {"id": 1, "slug": "4-koma", "name": "4-кома", "eng_name": "4-koma", "short_name": null, "site_ids": [1]},
                {"id": 9, "slug": "ln", "name": "Лайт-новелла", "eng_name": "Light novel", "short_name": "LN", "site_ids": [3]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn filters_by_site_ignoring_common_zero() {
        let c = sample();
        assert_eq!(c.genres_for(1).len(), 1);
        assert_eq!(c.genres_for(5).len(), 2);
        // Тип без site_ids нигде не показывается.
        let manga: Vec<_> = c.types_for(1).iter().map(|t| t.id).collect();
        assert_eq!(manga, vec![1]);
        assert_eq!(c.types_for(5).len(), 1);
        // Нуль в site_ids не делает статус общим: у AnimeLib «Выпуск прекращён» нет.
        assert_eq!(c.statuses_for(1).len(), 2);
        assert_eq!(c.statuses_for(5).len(), 1);
        assert!(c.statuses_for(3).is_empty());
    }

    #[test]
    fn anime_age_ratings_come_from_their_own_list() {
        let c = sample();
        assert_eq!(c.age_ratings_for(1)[0].label, "16+");
        assert_eq!(c.age_ratings_for(5)[0].label, "R-17");
    }

    #[test]
    fn term_helpers() {
        let c = sample();
        let hentai = c.genre(84).unwrap();
        assert!(hentai.is_blocked_in("ru"));
        assert!(!hentai.is_blocked_in("DE"));
        assert_eq!(hentai.anilist_id.as_deref(), Some("12"));
        assert_eq!(hentai.shiki_id.as_deref(), Some("1446"));
        assert!(c.genre(34).unwrap().matches("ACTION"));
        assert!(c.genre(34).unwrap().matches("бое"));
        assert!(!c.genre(34).unwrap().matches("драма"));
    }

    #[test]
    fn only_formats_one_to_seven_are_filterable() {
        let c = sample();
        assert!(c.formats[0].is_filterable());
        assert!(!c.formats[1].is_filterable());
        assert_eq!(c.formats_for(3).len(), 1);
    }

    #[test]
    fn missing_names_stay_empty() {
        let c: CatalogConstants = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c, CatalogConstants::default());
    }
}
