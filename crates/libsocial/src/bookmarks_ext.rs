//! Закладки сверх основ из `bookmarks.rs`: сортировка и поиск по списку, управление
//! папками, массовые операции, заметки к главам и серии, комментарий и пересмотры.
//!
//! Ловушки сервиса:
//! - у `GET /bookmarks` `user_id` обязателен (без него 422), `status=0` значит «все
//!   папки», а `meta.per_page` приходит массивом, поэтому размер страницы не фиксируют,
//!   а листают по `has_next`;
//! - поле `meta` закладки заполнено по-разному: у закладки без места и комментария
//!   `comment` приходит как `false`, а числа как `null`. Все поля здесь необязательные;
//! - менять и удалять можно только свои папки (id больше 50);
//! - у папок манги и аниме разные `site_ids`, а порядок папок касается только папок
//!   сайта запроса, поэтому записи про папки принимают `Site`;
//! - комментарий и пересмотры правятся тем же `POST /bookmarks`, что и папка.
//!   Без `item_id` и `progress` место чтения остаётся прежним;
//! - всё, что пишет на сервер, снято со скриптов сайта и вживую не проверялось.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bookmarks::{Bookmark, BookmarkFolder};
use crate::client::Client;
use crate::error::{Error, Result};
use crate::listing::{Page, SortOrder};
use crate::serde_util::{null_default, opt_u64_lenient, string_lenient};
use crate::site::Site;

/// Сколько записей помещается в заметку к одной главе или серии.
pub const NOTE_ENTRIES_MAX: usize = 15;

/// Папки с id не больше этого числа созданы сервисом, их нельзя менять и удалять.
pub const SYSTEM_FOLDER_MAX_ID: u64 = 50;

/// Чем отсортировать список закладок.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkSort {
    /// По названию, A–Я латиницей.
    Name,
    /// По русскому названию.
    RusName,
    /// По дате добавления в закладки.
    CreatedAt,
    /// По моей оценке.
    Rating,
    /// По дате выхода последней главы или серии.
    LastItemAt,
    /// По дате последнего чтения.
    UpdatedAt,
}

impl BookmarkSort {
    /// Значение `sort_by`. У аниме дата обновления называется иначе.
    pub fn as_str(self, site: Site) -> &'static str {
        match self {
            BookmarkSort::Name => "name",
            BookmarkSort::RusName => "rus_name",
            BookmarkSort::CreatedAt => "created_at",
            BookmarkSort::Rating => "rating",
            BookmarkSort::LastItemAt if site.is_anime() => "last_episode_at",
            BookmarkSort::LastItemAt => "last_chapter_at",
            BookmarkSort::UpdatedAt => "updated_at",
        }
    }
}

/// Запрос к списку закладок.
#[derive(Debug, Clone, Default)]
pub struct BookmarkListQuery {
    /// Чьи закладки. Обязателен: чужие видны, если папка публичная.
    pub user_id: u64,
    /// Папка. `0` или `None` – все.
    pub folder: Option<u64>,
    pub sort: Option<BookmarkSort>,
    pub order: Option<SortOrder>,
    /// Поиск по названию, от двух символов.
    pub text: String,
    pub page: u32,
}

impl BookmarkListQuery {
    fn to_query(&self, site: Site) -> Vec<(&'static str, String)> {
        let mut query = vec![
            ("user_id", self.user_id.to_string()),
            ("status", self.folder.unwrap_or(0).to_string()),
            ("page", self.page.max(1).to_string()),
        ];
        if let Some(sort) = self.sort {
            query.push(("sort_by", sort.as_str(site).into()));
        }
        if let Some(order) = self.order {
            query.push(("sort_type", order.as_str().into()));
        }
        let text = self.text.trim();
        if text.chars().count() >= 2 {
            query.push(("q", text.to_string()));
        }
        query
    }
}

/// Вид закладки. Нужен заметкам.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookmarkType {
    Manga,
    Anime,
}

impl BookmarkType {
    pub fn of(site: Site) -> BookmarkType {
        if site.is_anime() {
            BookmarkType::Anime
        } else {
            BookmarkType::Manga
        }
    }

    /// Значение `bookmark_type`, оно же поле `type` закладки.
    pub fn as_str(self) -> &'static str {
        match self {
            BookmarkType::Manga => "manga_bookmark",
            BookmarkType::Anime => "anime_bookmark",
        }
    }

    pub fn parse(s: &str) -> Option<BookmarkType> {
        match s {
            "manga_bookmark" => Some(BookmarkType::Manga),
            "anime_bookmark" => Some(BookmarkType::Anime),
            _ => None,
        }
    }

    /// К чему привязана заметка: глава или серия.
    pub fn item_type(self) -> &'static str {
        match self {
            BookmarkType::Manga => "chapter",
            BookmarkType::Anime => "episode",
        }
    }
}

/// Период пересмотра или перечитывания.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rewatch {
    /// Дата `ГГГГ-ММ-ДД`.
    #[serde(default, deserialize_with = "null_default")]
    pub start: String,
    /// Дата окончания; `None` значит «ещё не завершено».
    #[serde(default)]
    pub end: Option<String>,
}

impl Rewatch {
    pub fn new(start: impl Into<String>, end: Option<String>) -> Rewatch {
        Rewatch {
            start: start.into(),
            end,
        }
    }
}

/// Что изменить в папке. То, что `None`, не отправляется.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `#rrggbb`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
}

/// Одна запись заметки: место в главе или серии и текст.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteEntry {
    /// Номер страницы у манги, `ММ:СС` у аниме, абзац у ранобэ.
    #[serde(default, deserialize_with = "string_lenient", serialize_with = "serialize_position")]
    pub position: String,
    #[serde(default, deserialize_with = "null_default")]
    pub text: String,
}

/// Сайт шлёт номер страницы числом, а время серии строкой.
fn serialize_position<S: serde::Serializer>(p: &str, s: S) -> std::result::Result<S::Ok, S::Error> {
    match p.parse::<u64>() {
        Ok(n) => s.serialize_u64(n),
        Err(_) => s.serialize_str(p),
    }
}

/// Номер главы: том и номер. Для серии – сезон и номер.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteNumber {
    #[serde(default, deserialize_with = "string_lenient")]
    pub first: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub secondary: String,
}

/// Заметки к одной главе или серии.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteGroup {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub id: Option<u64>,
    /// Глава или серия, как есть.
    #[serde(default)]
    pub item: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub number: NoteNumber,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub notes: Vec<NoteEntry>,
}

/// Набор записей для главы или серии. Сохраняется целиком, прежние записи заменяются.
#[derive(Debug, Clone)]
pub struct NoteSave {
    pub bookmark_id: u64,
    pub kind: BookmarkType,
    /// Id главы (перевода в ветке) или серии.
    pub item_id: u64,
    /// Том или сезон.
    pub number_secondary: String,
    /// Номер главы или серии.
    pub number_first: String,
    pub entries: Vec<NoteEntry>,
}

impl NoteSave {
    fn to_body(&self) -> Result<Value> {
        if self.entries.is_empty() {
            return Err(Error::Other(
                "в заметке нет записей: чтобы убрать заметку, удалите её".into(),
            ));
        }
        if self.entries.len() > NOTE_ENTRIES_MAX {
            return Err(Error::Other(format!("в заметке больше {NOTE_ENTRIES_MAX} записей")));
        }
        if self
            .entries
            .iter()
            .any(|e| e.position.trim().is_empty() || e.text.trim().is_empty())
        {
            return Err(Error::Other("у каждой записи заметки нужны позиция и текст".into()));
        }
        Ok(json!({
            "bookmark_id": self.bookmark_id,
            "bookmark_type": self.kind.as_str(),
            "item_id": self.item_id,
            "item_type": self.kind.item_type(),
            "number_first": self.number_first,
            "number_secondary": self.number_secondary,
            "content": self.entries,
        }))
    }
}

/// Что изменить в закладке без смены папки и места.
#[derive(Debug, Clone, Default)]
pub struct BookmarkMetaUpdate {
    /// Пустая строка очищает комментарий.
    pub comment: Option<String>,
    /// Список целиком. Число пересмотров сервер считает сам.
    pub rewatches_history: Option<Vec<Rewatch>>,
}

impl Client {
    /// Список закладок с сортировкой и поиском. По 24–25 на страницу, общего числа
    /// нет. Сервер отдаёт только тайтлы того сайта, от имени которого спрашивают,
    /// поэтому папку манги собирают по MangaLib, SlashLib, RanobeLib и HentaiLib.
    /// Папки со всеми полями отдаёт `Client::bookmark_folders`.
    pub async fn bookmark_list(&self, site: Site, q: &BookmarkListQuery) -> Result<Page<Bookmark>> {
        self.get_page(site, "bookmarks", &q.to_query(site)).await
    }

    /// Создаёт папку. К какому сайту она относится, сервер определяет по `site`.
    pub async fn create_bookmark_folder(&self, site: Site, name: &str) -> Result<BookmarkFolder> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Other("у папки нет названия".into()));
        }
        self.send_data(
            site,
            Method::POST,
            "bookmarks/folder",
            &(),
            Some(&json!({ "name": name })),
        )
        .await
    }

    /// Правит свою папку и возвращает её новое состояние.
    pub async fn update_bookmark_folder(
        &self,
        site: Site,
        folder_id: u64,
        update: &FolderUpdate,
    ) -> Result<BookmarkFolder> {
        self.send_data(
            site,
            Method::PUT,
            &format!("bookmarks/folder/{folder_id}"),
            &(),
            Some(update),
        )
        .await
    }

    /// Удаляет свою папку. С `move_to` закладки переходят в другую папку, а что
    /// происходит с ними без него, неизвестно: сайт в этом случае параметр просто не
    /// шлёт. Для переноса он по умолчанию предлагает папку 1 у манги и 21 у аниме.
    pub async fn delete_bookmark_folder(&self, site: Site, folder_id: u64, move_to: Option<u64>) -> Result<()> {
        let body = match move_to {
            Some(to) => json!({ "move_to": to }),
            None => json!({}),
        };
        self.send_ok(
            site,
            Method::DELETE,
            &format!("bookmarks/folder/{folder_id}"),
            Some(&body),
        )
        .await
    }

    /// Новый порядок папок сайта: id в нужной последовательности. Папки других сайтов
    /// в список не входят.
    pub async fn reorder_bookmark_folders(&self, site: Site, order: &[u64]) -> Result<()> {
        self.send_ok(
            site,
            Method::PUT,
            "bookmarks/folder/order",
            Some(&json!({ "order": order })),
        )
        .await
    }

    /// Включает и выключает уведомления папок: пары «id папки, уведомлять ли».
    pub async fn set_bookmark_folder_notifications(&self, site: Site, folders: &[(u64, bool)]) -> Result<()> {
        let folders: Vec<Value> = folders
            .iter()
            .map(|(id, notify)| json!({"id": id, "notify": notify}))
            .collect();
        self.send_ok(
            site,
            Method::PUT,
            "bookmarks/folder/notifications",
            Some(&json!({ "folders": folders })),
        )
        .await
    }

    /// Кладёт сразу несколько тайтлов в папку. `media_ids` – числовые id тайтлов, а не
    /// slug. С пустым списком запрос не отправляется.
    pub async fn bookmarks_bulk_add(&self, site: Site, media_ids: &[u64], folder: u64) -> Result<()> {
        self.bulk(site, Method::POST, "bookmarks/bulk/store", media_ids, Some(folder))
            .await
    }

    /// Переносит несколько закладок в другую папку.
    pub async fn bookmarks_bulk_move(&self, site: Site, media_ids: &[u64], folder: u64) -> Result<()> {
        self.bulk(site, Method::PUT, "bookmarks/bulk", media_ids, Some(folder))
            .await
    }

    /// Убирает несколько тайтлов из закладок.
    pub async fn bookmarks_bulk_remove(&self, site: Site, media_ids: &[u64]) -> Result<()> {
        self.bulk(site, Method::DELETE, "bookmarks/bulk", media_ids, None).await
    }

    async fn bulk(&self, site: Site, method: Method, path: &str, media_ids: &[u64], folder: Option<u64>) -> Result<()> {
        if media_ids.is_empty() {
            return Ok(());
        }
        let mut body = json!({"media_type": site.catalog_path(), "media_ids": media_ids});
        if let Some(folder) = folder {
            body["status"] = folder.into();
        }
        self.send_ok(site, method, path, Some(&body)).await
    }

    /// Заметки закладки по главам и сериям. `kind` берётся из поля `type` закладки.
    pub async fn bookmark_notes(&self, bookmark_id: u64, kind: BookmarkType, page: u32) -> Result<Page<NoteGroup>> {
        let query = [
            ("bookmark_id", bookmark_id.to_string()),
            ("bookmark_type", kind.as_str().into()),
            ("page", page.max(1).to_string()),
        ];
        let site = if kind == BookmarkType::Anime {
            Site::AnimeLib
        } else {
            Site::MangaLib
        };
        self.get_page(site, "bookmarks/note", &query).await
    }

    /// Заметка одной главы или серии. `None`, если её ещё нет: сервер отвечает 404, а
    /// сайт молча показывает пустую форму.
    pub async fn bookmark_note(
        &self,
        bookmark_id: u64,
        kind: BookmarkType,
        number_first: &str,
        number_secondary: &str,
    ) -> Result<Option<NoteGroup>> {
        let query = [("number_first", number_first), ("number_secondary", number_secondary)];
        let site = if kind == BookmarkType::Anime {
            Site::AnimeLib
        } else {
            Site::MangaLib
        };
        // В пути стоит не id заметки, а составной идентификатор `chapter-{id закладки}`.
        let path = format!("bookmarks/note/{}-{bookmark_id}", kind.item_type());
        match self.get_data(site, &path, &query).await {
            Ok(note) => Ok(Some(note)),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Сохраняет заметку главы или серии: все записи сразу, до 15. Закладка должна
    /// существовать: сайт при необходимости сначала создаёт её через `set_bookmark`.
    pub async fn save_bookmark_note(&self, note: &NoteSave) -> Result<()> {
        let body = note.to_body()?;
        let site = if note.kind == BookmarkType::Anime {
            Site::AnimeLib
        } else {
            Site::MangaLib
        };
        self.send_ok(site, Method::POST, "bookmarks/note", Some(&body)).await
    }

    /// Удаляет заметку по её id.
    pub async fn delete_bookmark_note(&self, note_id: u64, bookmark_id: u64, kind: BookmarkType) -> Result<()> {
        let body = json!({"bookmark_id": bookmark_id, "bookmark_type": kind.as_str()});
        let site = if kind == BookmarkType::Anime {
            Site::AnimeLib
        } else {
            Site::MangaLib
        };
        self.send_ok(site, Method::DELETE, &format!("bookmarks/note/{note_id}"), Some(&body))
            .await
    }

    /// Меняет комментарий и историю пересмотров, не трогая место чтения. Папка нужна
    /// сервису в любом случае: передайте текущую. Возвращает обновлённую закладку.
    pub async fn update_bookmark_meta(
        &self,
        site: Site,
        slug_url: &str,
        folder: u64,
        update: &BookmarkMetaUpdate,
    ) -> Result<Bookmark> {
        let mut meta = json!({});
        if let Some(comment) = &update.comment {
            meta["comment"] = comment.trim().into();
        }
        if let Some(history) = &update.rewatches_history {
            meta["rewatches_history"] = serde_json::to_value(history).expect("список дат сериализуется");
        }
        let body = json!({
            "media_type": site.catalog_path(),
            "media_slug": slug_url,
            "bookmark": {"status": folder},
            "meta": meta,
        });
        self.send_data(site, Method::POST, "bookmarks", &(), Some(&body)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_name_depends_on_site() {
        assert_eq!(BookmarkSort::LastItemAt.as_str(Site::MangaLib), "last_chapter_at");
        assert_eq!(BookmarkSort::LastItemAt.as_str(Site::AnimeLib), "last_episode_at");
        assert_eq!(BookmarkSort::UpdatedAt.as_str(Site::SlashLib), "updated_at");
    }

    #[test]
    fn list_query_defaults_to_all_folders() {
        let q = BookmarkListQuery {
            user_id: 5,
            text: "а".into(),
            ..Default::default()
        }
        .to_query(Site::MangaLib);
        assert_eq!(
            q,
            vec![
                ("user_id", "5".to_string()),
                ("status", "0".to_string()),
                ("page", "1".to_string())
            ]
        );
    }

    #[test]
    fn meta_tolerates_both_shapes() {
        use crate::bookmarks::BookmarkMeta;

        let bare: BookmarkMeta =
            serde_json::from_value(json!({"comment": false, "rewatches": null, "item_number": null})).unwrap();
        assert!(bare.comment.is_none() && bare.rewatches.is_none() && bare.notes == 0);
        let full: BookmarkMeta = serde_json::from_value(json!({
            "item_number": 11, "rewatches": 1, "comment": "текст", "notes": 2,
            "rewatches_history": [{"start": "2026-01-01", "end": null}]
        }))
        .unwrap();
        assert_eq!(full.comment.as_deref(), Some("текст"));
        assert_eq!(full.rewatches_history[0].end, None);
    }

    #[test]
    fn note_validation_and_position_encoding() {
        let mut note = NoteSave {
            bookmark_id: 1,
            kind: BookmarkType::Manga,
            item_id: 2,
            number_secondary: "1".into(),
            number_first: "3".into(),
            entries: vec![],
        };
        assert!(note.to_body().is_err());
        note.entries.push(NoteEntry {
            position: "5".into(),
            text: "текст".into(),
        });
        note.entries.push(NoteEntry {
            position: "01:30".into(),
            text: "ещё".into(),
        });
        let body = note.to_body().unwrap();
        assert_eq!(body["content"][0]["position"], 5);
        assert_eq!(body["content"][1]["position"], "01:30");
        assert_eq!(body["item_type"], "chapter");
        note.entries = vec![
            NoteEntry {
                position: "1".into(),
                text: "т".into()
            };
            NOTE_ENTRIES_MAX + 1
        ];
        assert!(note.to_body().is_err());
    }
}
