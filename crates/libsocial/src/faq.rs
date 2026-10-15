//! Справка сайта: разделы и статьи. Общая для всей сети, работает без входа.
//!
//! Текст статьи – документ ProseMirror. Картинки в нём лежат в `attrs.src` путём на
//! хосте API (`/uploads/posts/…`), причём путь приходит в лишних кавычках с обратной
//! косой (`\"/uploads/…\"`). Клиент снимает кавычки и делает адрес полным.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::Result;
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;

/// Раздел справки. Подраздел ссылается на родителя через `category_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaqCategory {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub category_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub root_category_id: Option<u64>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub posts_count: u64,
}

/// «Полезно» и «бесполезно» у статьи.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaqVotes {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub up: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub down: u64,
}

/// Статья в списке раздела.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaqArticleShort {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Путь для `faq_article`: `33--vozmoznosti-v-kommentariiax`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub updated_at: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::opt_lenient")]
    pub category: Option<FaqCategory>,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: FaqVotes,
}

/// Статья целиком.
#[derive(Debug, Clone)]
pub struct FaqArticle {
    pub item: FaqArticleShort,
    /// Текст с полными адресами картинок.
    pub content: Doc,
    /// Документ как пришёл.
    pub content_raw: Value,
}

#[derive(Deserialize)]
struct RawArticle {
    #[serde(flatten)]
    item: FaqArticleShort,
    #[serde(default)]
    content: Value,
}

impl Client {
    /// Все разделы справки списком, подразделы вместе с разделами.
    pub async fn faq_categories(&self) -> Result<Vec<FaqCategory>> {
        self.get_data(Site::MangaLib, "faq/category", &()).await
    }

    /// Статьи раздела. `with_subcategories` добавляет статьи его подразделов.
    pub async fn faq_articles(&self, category_id: u64, with_subcategories: bool) -> Result<Vec<FaqArticleShort>> {
        let query = [
            ("category_id", category_id.to_string()),
            ("include_sub_categories", u8::from(with_subcategories).to_string()),
        ];
        self.get_data(Site::MangaLib, "faq/articles", &query).await
    }

    /// Статья по `slug_url`.
    pub async fn faq_article(&self, slug_url: &str) -> Result<FaqArticle> {
        let raw: RawArticle = self
            .get_data(Site::MangaLib, &format!("faq/articles/{slug_url}"), &())
            .await?;
        let mut content = raw.content.clone();
        absolute_images(&mut content, &|p| self.upload_url(p));
        Ok(FaqArticle {
            item: raw.item,
            content: Doc::from_prosemirror(&content),
            content_raw: raw.content,
        })
    }
}

/// Снимает с `attrs.src` картинок лишние кавычки и делает адрес полным.
fn absolute_images(node: &mut Value, absolute: &dyn Fn(&str) -> String) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(|n| absolute_images(n, absolute)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && let Some(src) = map.get_mut("attrs").and_then(|a| a.get_mut("src"))
                && let Some(path) = src.as_str()
            {
                let path = path.trim_matches(|c| c == '"' || c == '\\');
                // Пустой путь стал бы адресом корня хоста.
                *src = if path.is_empty() {
                    Value::Null
                } else {
                    Value::String(absolute(path))
                };
            }
            if let Some(children) = map.get_mut("content") {
                absolute_images(children, absolute);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::Block;
    use serde_json::json;

    #[test]
    fn image_paths_lose_quotes() {
        let mut doc = json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "Текст"}]},
            {"type": "image", "attrs": {"src": "\\\"/uploads/posts/a.png\\\""}}
        ]});
        absolute_images(&mut doc, &|p| format!("https://api.example{p}"));
        assert_eq!(
            doc["content"][1]["attrs"]["src"],
            "https://api.example/uploads/posts/a.png"
        );
        let parsed = Doc::from_prosemirror(&doc);
        let Block::Images(pictures) = &parsed.blocks[1] else {
            panic!("{:?}", parsed.blocks);
        };
        assert_eq!(pictures[0].url, "https://api.example/uploads/posts/a.png");
    }
}
