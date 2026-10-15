//! Жалобы: на пользователя, подборку, отзыв, карточку, тему форума, главу и серию.
//!
//! Жалоба на комментарий идёт своим путём (`Client::report_comment`). Остальные
//! уходят на общий `POST /reports`. Причины жалоб у каждого вида объекта свои: сайт
//! спрашивает их по пространству имён (`namespaces`), и это же пространство кладёт
//! в саму жалобу. `reportable_type` у главы и серии – их `model` из ответа сервиса,
//! а не `post_type` комментариев.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::error::Result;
use crate::serde_util::{null_default, string_lenient, u64_lenient};
use crate::site::Site;

/// Пространство имён причин жалоб на комментарии.
pub(crate) const COMMENT_NAMESPACE: &str = "social_comments";

/// Причина жалобы.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportType {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Это значение уходит в жалобу (`reason`), а не `id`.
    #[serde(deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    /// Для каких видов объектов причина годится. Одна строка тоже принимается.
    #[serde(default, deserialize_with = "namespaces_lenient")]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

impl ReportType {
    /// Нужно ли пояснение к жалобе.
    pub fn comment_required(&self) -> bool {
        self.metadata
            .as_ref()
            .and_then(|m| m.get("comment_required"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// На что жалоба.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportTarget {
    User(u64),
    Collection(u64),
    Review(u64),
    Card(u64),
    /// Тема форума.
    Discussion(u64),
    /// Ошибка в главе: недостающие, повторённые или перепутанные страницы. У ранобэ
    /// (`Site::RanobeLib`) свои причины.
    Chapter {
        chapter_id: u64,
        site: Site,
    },
    /// Серия аниме. Сайт отправляет такую жалобу из выбора плеера, с пространством
    /// имён про недостающий вид перевода.
    Episode(u64),
}

impl ReportTarget {
    fn id(self) -> u64 {
        match self {
            ReportTarget::User(id)
            | ReportTarget::Collection(id)
            | ReportTarget::Review(id)
            | ReportTarget::Card(id)
            | ReportTarget::Discussion(id)
            | ReportTarget::Episode(id) => id,
            ReportTarget::Chapter { chapter_id, .. } => chapter_id,
        }
    }

    fn reportable_type(self) -> &'static str {
        match self {
            ReportTarget::User(_) => "user",
            ReportTarget::Collection(_) => "collection",
            ReportTarget::Review(_) => "review",
            ReportTarget::Card(_) => "card",
            ReportTarget::Discussion(_) => "discussion",
            ReportTarget::Chapter { .. } => "chapter",
            ReportTarget::Episode(_) => "episodes",
        }
    }

    /// Пространство имён причин, оно же уходит в жалобу.
    pub fn namespace(self) -> &'static str {
        match self {
            ReportTarget::User(_) => "user-profile",
            ReportTarget::Collection(_) => "collection",
            ReportTarget::Review(_) => "review",
            ReportTarget::Card(_) => "card",
            ReportTarget::Discussion(_) => "discussion",
            ReportTarget::Chapter {
                site: Site::RanobeLib, ..
            } => "reader-book",
            ReportTarget::Chapter { .. } => "reader-manga",
            ReportTarget::Episode(_) => "episode-empty-translation-type",
        }
    }
}

/// Пространства имён, причины которых сайт показывает вместе (`Tr` в его скриптах).
fn related_namespaces(namespace: &str) -> Vec<&str> {
    match namespace {
        "discussion" | "social_forum" => vec!["discussion", "social_forum"],
        other => vec![other],
    }
}

fn namespaces_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => vec![s],
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    })
}

impl Client {
    /// Причины жалоб на такой объект. `ReportType::name` уходит в `report`.
    pub async fn report_reasons_for(&self, target: ReportTarget) -> Result<Vec<ReportType>> {
        self.report_reasons(target.namespace()).await
    }

    /// Причины жалоб одного пространства имён. Из ответа сайт оставляет только
    /// причины, чьи `namespaces` пересекаются с нужными, причём тема форума делит
    /// причины с `social_forum`. Клиент отбирает так же.
    pub(crate) async fn report_reasons(&self, namespace: &str) -> Result<Vec<ReportType>> {
        // Ответ – голый массив, без обёртки `data`. Причина, которая не разобралась,
        // выбрасывается, а не валит весь список.
        let raw: Vec<Value> = self
            .get_json(Site::MangaLib, "reports/types", &[("namespaces[]", namespace)])
            .await?;
        let wanted = related_namespaces(namespace);
        Ok(raw
            .into_iter()
            .filter_map(|v| serde_json::from_value::<ReportType>(v).ok())
            .filter(|r| r.namespaces.iter().any(|n| wanted.contains(&n.as_str())))
            .collect())
    }

    /// Отправляет жалобу. `reason` – `ReportType::name`, `note` – пояснение, у части
    /// причин обязательное (`ReportType::comment_required`).
    pub async fn report(&self, target: ReportTarget, reason: &str, note: &str) -> Result<()> {
        let body = json!({
            "reason": reason,
            "metadata": {"comment": note},
            "reportable_id": target.id(),
            "reportable_type": target.reportable_type(),
            "namespaces": target.namespace(),
        });
        self.send_ok(Site::MangaLib, Method::POST, "reports", Some(&body)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_map_to_type_and_namespace() {
        let cases = [
            (ReportTarget::User(1), "user", "user-profile"),
            (ReportTarget::Collection(1), "collection", "collection"),
            (ReportTarget::Review(1), "review", "review"),
            (ReportTarget::Card(1), "card", "card"),
            (ReportTarget::Discussion(1), "discussion", "discussion"),
            (
                ReportTarget::Chapter {
                    chapter_id: 1,
                    site: Site::MangaLib,
                },
                "chapter",
                "reader-manga",
            ),
            (ReportTarget::Episode(1), "episodes", "episode-empty-translation-type"),
        ];
        for (target, kind, namespace) in cases {
            assert_eq!(
                (target.reportable_type(), target.namespace()),
                (kind, namespace),
                "{target:?}"
            );
        }
    }

    #[test]
    fn chapter_namespace_depends_on_site() {
        let chapter = |site| ReportTarget::Chapter { chapter_id: 1, site };
        assert_eq!(chapter(Site::MangaLib).namespace(), "reader-manga");
        assert_eq!(chapter(Site::SlashLib).namespace(), "reader-manga");
        assert_eq!(chapter(Site::RanobeLib).namespace(), "reader-book");
    }

    #[test]
    fn reason_namespaces_parse() {
        let r: ReportType = serde_json::from_value(serde_json::json!({
            "id": "4", "name": 4, "label": "Спам", "namespaces": ["review", "collection"],
            "metadata": {"comment_required": true}
        }))
        .unwrap();
        assert_eq!(r.name, "4");
        assert_eq!(r.namespaces, ["review", "collection"]);
        assert!(r.comment_required());
    }
}
