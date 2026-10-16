//! Общее для всех списков API: страница с признаком продолжения, порядок сортировки
//! и терпимые разборщики значений, которые сервер присылает в разном виде.
//!
//! Списки сервиса пагинируются по-разному. У одних есть `meta.total` и `links.next`,
//! у других только `links.next`, у третьих (сообщения короткого диалога, общие друзья,
//! топ-100 пользователей) нет ни `meta`, ни `links`. Форум отвечает плоским пагинатором
//! Laravel: номера страниц и `next_page_url` лежат рядом с `data`, а `links` – массив
//! кнопок. `Page` справляется со всеми четырьмя.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::Result;
use crate::serde_util::lenient_vec;
use crate::site::Site;

/// Порядок сортировки: параметр `sort_type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
}

impl SortOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            SortOrder::Asc => "asc",
            SortOrder::Desc => "desc",
        }
    }
}

/// Страница любого списка API. Элемент, который не разобрался, в неё не попадает.
///
/// Разбирается прямо из ответа сервера `{"data": [...], "links": ..., "meta": ...}`
/// любого из четырёх видов: курсорного (только `links.next`), полного (с `last_page` и
/// `total`), списка целиком без `links` и `meta` и плоского, где номера страниц лежат
/// на верхнем уровне ответа.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(from = "RawPage<T>", bound(deserialize = "T: DeserializeOwned"))]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Есть ли следующая страница. Сервер говорит об этом по-разному, учитывается всё:
    /// непустой `links.next`, `meta.has_next_page`, `meta.next_page_url` (в закладках
    /// он булев), непустой `next_page_url` плоского ответа и `current_page < last_page`.
    /// У аниме `has_next_page` нет вовсе.
    pub has_next: bool,
    /// Общее число записей (`meta.total` или `meta.items_count`, у плоского ответа
    /// `total`). У большинства списков его нет, при поиске `q` обычно пропадает:
    /// листать надо до `has_next`.
    pub total: Option<u64>,
    /// Номер страницы по `meta.current_page` (у плоского ответа `current_page`).
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub last_page: Option<u32>,
    /// `meta` ответа целиком. В нём то, что у каждого списка своё: курсор случайного
    /// порядка каталога (`catalog::RandomCursor::of`), фильтры и лимиты карточек.
    pub meta: Value,
}

impl<T> Page<T> {
    /// Страница, которой на сервере нет, или пустой результат.
    pub fn empty() -> Page<T> {
        Page {
            items: Vec::new(),
            has_next: false,
            total: None,
            page: None,
            per_page: None,
            last_page: None,
            meta: Value::Null,
        }
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Page::empty()
    }
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct RawPage<T> {
    #[serde(default, deserialize_with = "lenient_vec")]
    data: Vec<T>,
    #[serde(default)]
    links: Value,
    #[serde(default)]
    meta: Value,
    /// Поля плоского пагинатора Laravel (форум). У остальных списков их нет.
    #[serde(default)]
    current_page: Value,
    #[serde(default)]
    last_page: Value,
    #[serde(default)]
    per_page: Value,
    #[serde(default)]
    total: Value,
    #[serde(default)]
    next_page_url: Value,
}

impl<T> From<RawPage<T>> for Page<T> {
    fn from(raw: RawPage<T>) -> Page<T> {
        let meta = raw.meta;
        // Число берётся из `meta`, а если его там нет, с верхнего уровня ответа.
        let number = |key: &str, flat: &Value| meta_u64(&meta, key).or_else(|| value_u64(flat));
        let small = |key: &str, flat: &Value| number(key, flat).and_then(|n| u32::try_from(n).ok());
        let page = small("current_page", &raw.current_page);
        let last_page = small("last_page", &raw.last_page);
        let by_numbers = matches!((page, last_page), (Some(cur), Some(last)) if cur < last);
        // У плоского ответа `next_page_url` – адрес. В `meta` закладок под тем же именем лежит флаг.
        let flat_next = raw.next_page_url.as_str().is_some_and(|s| !s.is_empty());
        Page {
            items: raw.data,
            has_next: has_next(&raw.links, &meta) || flat_next || by_numbers,
            total: number("total", &raw.total).or_else(|| meta_u64(&meta, "items_count")),
            page,
            per_page: small("per_page", &raw.per_page),
            last_page,
            meta,
        }
    }
}

/// Признак следующей страницы по `links.next`, `meta.has_next_page` и
/// `meta.next_page_url`. Нужен и спискам, которые не укладываются в `Page`
/// (дерево комментариев).
pub(crate) fn has_next(links: &Value, meta: &Value) -> bool {
    // `links.next` – адрес следующей страницы, флаги в `meta` – булевы значения,
    // которые сервер присылает и как `"0"` или `"false"`.
    let link = links.get("next").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    let flag = |key: &str| {
        meta.get(key)
            .and_then(|v| bool_lenient(v.clone()).ok())
            .unwrap_or(false)
    };
    link || flag("has_next_page") || flag("next_page_url")
}

/// Число из `meta`: сервер присылает его то числом, то строкой.
fn meta_u64(meta: &Value, key: &str) -> Option<u64> {
    value_u64(meta.get(key)?)
}

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Client {
    /// GET списка с разбором в `Page`.
    pub(crate) async fn get_page<T, Q>(&self, site: Site, path: &str, query: &Q) -> Result<Page<T>>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        self.get_json(site, path, query).await
    }
}

/// Булево значение, которое сервер присылает и как `true`, и как `1`, и как `"1"`.
pub fn bool_lenient<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    Ok(opt_bool_lenient(d)?.unwrap_or(false))
}

/// То же, но `null` и непонятное значение дают `None`.
pub fn opt_bool_lenient<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<bool>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => Some(b),
        Value::Number(n) => n.as_f64().map(|n| n != 0.0),
        Value::String(s) => match s.trim() {
            "1" | "true" => Some(true),
            "0" | "false" | "" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

/// Число или строка с числом, не влезающее в `u8` значение отбрасывается.
pub fn opt_u8_lenient<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<u8>, D::Error> {
    Ok(crate::serde_util::opt_u64_lenient(d)?.and_then(|n| u8::try_from(n).ok()))
}

/// Список id сайтов: массив, объект с числовыми значениями или строки с числами.
pub fn site_ids_lenient<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
    let items: Vec<Value> = match Value::deserialize(d)? {
        Value::Array(a) => a,
        Value::Object(o) => o.into_iter().map(|(_, v)| v).collect(),
        _ => Vec::new(),
    };
    Ok(items
        .iter()
        .filter_map(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .filter_map(|n| u8::try_from(n).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Page<u64> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn page_shapes() {
        let full = parse(json!({"data": [1, 2, "x"], "links": {"next": "u"}, "meta": {"total": 7, "current_page": 2}}));
        assert_eq!(full.items, vec![1, 2]);
        assert!(full.has_next);
        assert_eq!(full.total, Some(7));
        assert_eq!(full.page, Some(2));

        let last = parse(json!({"data": [1], "links": {"next": null}, "meta": {"items_count": 3}}));
        assert!(!last.has_next);
        assert_eq!(last.total, Some(3));

        let bare = parse(json!({"data": []}));
        assert!(!bare.has_next && bare.items.is_empty() && bare.total.is_none());

        let flagged = parse(json!({"data": [1], "meta": {"next_page_url": true}}));
        assert!(flagged.has_next);
        let denied = parse(json!({"data": [1], "meta": {"has_next_page": "false", "next_page_url": "0"}}));
        assert!(!denied.has_next);

        // Полный список: конец по номерам, число строкой.
        let numbered = parse(
            json!({"data": [], "links": {"next": null}, "meta": {"current_page": 2, "last_page": 9, "total": "180", "per_page": 20}}),
        );
        assert!(numbered.has_next);
        assert_eq!(
            (numbered.total, numbered.per_page, numbered.last_page),
            (Some(180), Some(20), Some(9))
        );
        let end = parse(json!({"data": [], "meta": {"current_page": 9, "last_page": 9}}));
        assert!(!end.has_next);

        // Список целиком: `meta` и `links` пустыми массивами.
        let flat = parse(json!({"data": [7], "meta": [], "links": []}));
        assert_eq!(flat.items, vec![7]);
        assert!(!flat.has_next && flat.page.is_none());
    }

    #[test]
    fn laravel_flat_page() {
        // Форум: номера страниц рядом с `data`, `links` – массив кнопок пагинации.
        let first = parse(json!({
            "current_page": 1, "data": [1, 2], "first_page_url": "https://api/forum/discussion?page=1",
            "from": 1, "last_page": 50839, "links": [{"url": null, "label": "&laquo; Previous", "active": false}],
            "next_page_url": "https://api/forum/discussion?page=2", "path": "https://api/forum/discussion",
            "per_page": 10, "prev_page_url": null, "to": 10, "total": 508384
        }));
        assert_eq!(first.items, vec![1, 2]);
        assert!(first.has_next);
        assert_eq!(
            (first.page, first.last_page, first.per_page, first.total),
            (Some(1), Some(50839), Some(10), Some(508384))
        );

        // Последняя страница: адреса следующей нет, номера совпадают.
        let last = parse(json!({"current_page": "3", "data": [1], "last_page": 3, "next_page_url": null, "total": 21}));
        assert!(!last.has_next);
        assert_eq!((last.page, last.total), (Some(3), Some(21)));

        // Только адрес следующей страницы, без номеров.
        let linked = parse(json!({"data": [], "next_page_url": "u"}));
        assert!(linked.has_next && linked.page.is_none());

        // `meta` важнее верхнего уровня, если есть оба.
        let both = parse(json!({"data": [], "current_page": 7, "meta": {"current_page": 2, "last_page": 2}}));
        assert_eq!(both.page, Some(2));
        assert!(!both.has_next);
    }

    #[test]
    fn lenient_values() {
        #[derive(Deserialize)]
        struct T {
            #[serde(deserialize_with = "bool_lenient")]
            a: bool,
            #[serde(deserialize_with = "bool_lenient")]
            b: bool,
            #[serde(deserialize_with = "site_ids_lenient")]
            ids: Vec<u8>,
        }
        let t: T = serde_json::from_value(json!({"a": 1, "b": "0", "ids": {"0": "1", "1": 5}})).unwrap();
        assert!(t.a && !t.b);
        assert_eq!(t.ids, vec![1, 5]);
    }
}
