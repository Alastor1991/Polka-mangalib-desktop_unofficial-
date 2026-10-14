//! Общее для списков раздела «аккаунт и общение»: страница с признаком продолжения,
//! порядок сортировки и терпимые разборщики булевых значений.
//!
//! Списки сервиса пагинируются по-разному. У одних есть `meta.total` и `links.next`,
//! у других только `links.next`, у третьих (сообщения короткого диалога, общие друзья,
//! топ-100 пользователей) нет ни `meta`, ни `links`. `Page` справляется со всеми тремя.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::Result;
use crate::serde_util::lenient_vec;
use crate::site::Site;

/// Порядок сортировки: параметр `sort_type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// Страница списка. Элемент, который не разобрался, в неё не попадает.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Есть ли следующая страница: `meta.has_next_page` или непустой `links.next`.
    pub has_next: bool,
    /// Общее число записей, если сервер его прислал (`meta.total` или `meta.items_count`).
    /// У большинства списков его нет, листать надо до пустого `has_next`.
    pub total: Option<u64>,
    /// Номер страницы по `meta.current_page`.
    pub page: Option<u32>,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Page {
            items: Vec::new(),
            has_next: false,
            total: None,
            page: None,
        }
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
}

impl<T: DeserializeOwned> Page<T> {
    fn from_raw(raw: RawPage<T>) -> Page<T> {
        let meta = &raw.meta;
        let flag = |key: &str| meta.get(key).and_then(Value::as_bool).unwrap_or(false);
        let next = raw.links.get("next").is_some_and(|n| !n.is_null());
        Page {
            items: raw.data,
            // `next_page_url` в закладках приходит булевым значением.
            has_next: flag("has_next_page") || flag("next_page_url") || next,
            total: ["total", "items_count"]
                .iter()
                .find_map(|k| meta.get(k).and_then(Value::as_u64)),
            page: meta
                .get("current_page")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok()),
        }
    }
}

impl Client {
    /// GET списка с разбором `{"data": [...], "links": ..., "meta": ...}`.
    pub(crate) async fn get_page<T, Q>(&self, site: Site, path: &str, query: &Q) -> Result<Page<T>>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        let raw: RawPage<T> = self.get_json(site, path, query).await?;
        Ok(Page::from_raw(raw))
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
        Page::from_raw(serde_json::from_value::<RawPage<u64>>(v).unwrap())
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
