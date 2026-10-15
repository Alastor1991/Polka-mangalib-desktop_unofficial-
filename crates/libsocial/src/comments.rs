//! Комментарии: к тайтлу, главе и отдельной её странице, серии, отзыву, подборке,
//! новости, теме форума, карточке. Голоса, ответы любой глубины, жалобы, стикеры.
//!
//! Сервер отдаёт страницу корневых комментариев и плоский список всех ответов к
//! ним. Дерево собирается на клиенте по `parent_comment`. Комментарии не зависят
//! от `Site-Id`: id всех объектов в сети общие.
//!
//! Подписка на новые комментарии описывает объект иначе, чем `Target`: тайтлом и
//! необязательным id главы или серии (`CommentWatch`).

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cards::TitleKind;
use crate::client::{Auth, Client, join_url, record_of};
use crate::error::{Error, Result};
use crate::listing::Page;
use crate::listing::bool_lenient;
use crate::reports::COMMENT_NAMESPACE;
pub use crate::reports::ReportType;
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_u64_lenient, u64_lenient};
use crate::site::Site;

/// К чему комментарий.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    /// Тайтл манги, ранобэ или комикса (id тайтла).
    Manga(u64),
    Anime(u64),
    /// Глава: id перевода в ветке. С `page` – комментарии к одной странице главы.
    Chapter {
        chapter_id: u64,
        page: Option<u64>,
    },
    Episode(u64),
    /// Озвучка главы манги.
    ChapterPlayer(u64),
    Review(u64),
    Collection(u64),
    /// Новость.
    Post(u64),
    Discussion(u64),
    Card(u64),
}

impl Target {
    pub fn post_type(self) -> &'static str {
        match self {
            Target::Manga(_) => "manga",
            Target::Anime(_) => "anime",
            Target::Chapter { .. } => "chapter",
            Target::Episode(_) => "episodes",
            Target::ChapterPlayer(_) => "chapter_player",
            Target::Review(_) => "review",
            Target::Collection(_) => "collection",
            Target::Post(_) => "post",
            Target::Discussion(_) => "discussion",
            Target::Card(_) => "card",
        }
    }

    pub fn post_id(self) -> u64 {
        match self {
            Target::Manga(id)
            | Target::Anime(id)
            | Target::Episode(id)
            | Target::ChapterPlayer(id)
            | Target::Review(id)
            | Target::Collection(id)
            | Target::Post(id)
            | Target::Discussion(id)
            | Target::Card(id) => id,
            Target::Chapter { chapter_id, .. } => chapter_id,
        }
    }

    pub fn page(self) -> Option<u64> {
        match self {
            Target::Chapter { page, .. } => page,
            _ => None,
        }
    }

    /// Обратно из полей комментария (`relation_type`, `relation_id`, `post_page`).
    pub fn from_relation(kind: &str, id: u64, page: Option<u64>) -> Option<Target> {
        Some(match kind {
            "manga" => Target::Manga(id),
            "anime" => Target::Anime(id),
            "chapter" => Target::Chapter { chapter_id: id, page },
            "episodes" => Target::Episode(id),
            "chapter_player" => Target::ChapterPlayer(id),
            "review" => Target::Review(id),
            "collection" => Target::Collection(id),
            "post" => Target::Post(id),
            "discussion" => Target::Discussion(id),
            "card" => Target::Card(id),
            _ => return None,
        })
    }

    fn query(self) -> Vec<(&'static str, String)> {
        let mut q = vec![
            ("post_type", self.post_type().to_string()),
            ("post_id", self.post_id().to_string()),
        ];
        if let Some(page) = self.page() {
            q.push(("post_page", page.to_string()));
        }
        q
    }
}

/// Порядок списка, как на сайте.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommentSort {
    #[default]
    Newest,
    Oldest,
    Popular,
}

impl CommentSort {
    fn params(self) -> [(&'static str, &'static str); 2] {
        match self {
            CommentSort::Newest => [("sort_by", "id"), ("sort_type", "desc")],
            CommentSort::Oldest => [("sort_by", "id"), ("sort_type", "asc")],
            CommentSort::Popular => [("sort_by", "votes_up"), ("sort_type", "desc")],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Avatar {
    #[serde(default)]
    pub url: Option<String>,
}

/// Рамка аватара. Пути относительные, от `https://cover.cdnlibs.org`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarFrame {
    #[serde(default)]
    pub sm: Option<String>,
    #[serde(default)]
    pub md: Option<String>,
}

/// «Серия посещений» автора.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginStreak {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub login_streak: u64,
    #[serde(default)]
    pub last_login_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Premium {
    #[serde(default, deserialize_with = "null_default")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<Avatar>,
    #[serde(default)]
    pub avatar_frame: Option<AvatarFrame>,
    #[serde(default)]
    pub login_streak: Option<LoginStreak>,
    #[serde(default)]
    pub premium: Option<Premium>,
}

/// Голоса. Свой голос приходит только вошедшему.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Votes {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub up: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub down: u64,
    /// `Some(Vote::Up)`, `Some(Vote::Down)` или `None`, если не голосовал.
    #[serde(default, deserialize_with = "vote_lenient")]
    pub user: Option<Vote>,
}

impl Votes {
    /// Рейтинг одним числом, как показывает сайт.
    pub fn score(&self) -> i64 {
        self.up as i64 - self.down as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    Up,
    Down,
}

impl Vote {
    fn value(self) -> u8 {
        match self {
            Vote::Up => 1,
            Vote::Down => 0,
        }
    }
}

fn vote_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vote>, D::Error> {
    Ok(match opt_u64_lenient(d)? {
        Some(1) => Some(Vote::Up),
        Some(0) => Some(Vote::Down),
        _ => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Корень ветки. У корня `None`.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub root_id: Option<u64>,
    /// Родитель. У корня `None`.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub parent_comment: Option<u64>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comment_level: u64,
    /// Страница главы, к которой оставлен комментарий.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub post_page: Option<u64>,
    /// Текст в HTML, как присылает сервер. Разобранный – `Comment::text`.
    #[serde(default, deserialize_with = "null_default")]
    pub comment: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub relation_type: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub relation_id: u64,
    #[serde(default)]
    pub user: Option<Author>,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: Votes,
    /// Объект, к которому комментарий: тайтл, глава, серия. Как есть.
    #[serde(default)]
    pub relation: Option<Value>,
    /// Есть только у закреплённых: команда, от имени которой закреплено.
    #[serde(default)]
    pub sticky_info: Option<Value>,
}

impl Comment {
    pub fn text(&self) -> Doc {
        Doc::from_html(&self.comment)
    }

    /// К чему комментарий.
    pub fn target(&self) -> Option<Target> {
        Target::from_relation(&self.relation_type, self.relation_id, self.post_page)
    }

    pub fn is_root(&self) -> bool {
        self.parent_comment.is_none()
    }
}

/// Страница комментариев: корни и плоский список ответов к ним всех уровней.
#[derive(Debug, Clone, Default)]
pub struct CommentPage {
    pub roots: Vec<Comment>,
    /// Ответы. Бывают ответы к корням с других страниц и повторы: собирать дерево
    /// надо только от корней и без дублей (`polka_core` делает это сам).
    pub replies: Vec<Comment>,
    pub has_next: bool,
}

#[derive(Deserialize)]
struct RawPage {
    data: RawTree,
    #[serde(default)]
    links: Value,
    #[serde(default)]
    meta: Value,
}

#[derive(Deserialize)]
struct RawTree {
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    root: Vec<Comment>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    replies: Vec<Comment>,
}

/// Новый комментарий или ответ.
#[derive(Debug, Clone)]
pub struct NewComment {
    pub target: Target,
    pub text: Doc,
    /// Ответ на этот комментарий.
    pub reply_to: Option<Comment>,
    /// Картинки, загруженные через `upload_comment_images`, в порядке показа.
    pub images: Vec<Uploaded>,
    /// Стикер из `sticker_packs`, один на комментарий.
    pub sticker: Option<Sticker>,
}

/// Загруженная на сервер картинка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploaded {
    pub name: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "previewUrl")]
    pub preview_url: Option<String>,
    #[serde(default, rename = "originalUrl")]
    pub original_url: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub width: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub height: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerImages {
    #[serde(default)]
    pub orig: Option<String>,
    #[serde(default)]
    pub sm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(deserialize_with = "u64_lenient")]
    pub sticker_pack_id: u64,
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub extension: String,
    #[serde(default)]
    pub shortcut: Option<String>,
    /// Пути от `https://cover.cdnlibs.org`.
    #[serde(default)]
    pub images: Option<StickerImages>,
    #[serde(default, deserialize_with = "null_default")]
    pub associations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StickerPack {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub title: String,
    #[serde(default, deserialize_with = "null_default")]
    pub stickers: Vec<Sticker>,
}

/// Что показывать в списке комментариев пользователя.
#[derive(Debug, Clone, Default)]
pub struct UserCommentsQuery {
    pub page: u32,
    /// Популярные сверху, иначе новые.
    pub by_votes: bool,
    pub ascending: bool,
    /// Только к объектам этого вида (`manga`, `chapter`...), `None` – все.
    pub post_type: Option<String>,
    pub with_images: bool,
    /// Поиск по тексту, от двух символов.
    pub text: String,
}

/// Объект подписки на новые комментарии: тайтл целиком или его глава и серия.
/// У главы и серии сайт передаёт их тайтл и отдельно id главы или серии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommentWatch {
    /// `manga` или `anime` в запросе.
    pub kind: TitleKind,
    pub title_id: u64,
    /// Глава или серия; `None` – тайтл целиком.
    pub content_id: Option<u64>,
}

impl CommentWatch {
    pub fn manga(title_id: u64) -> CommentWatch {
        CommentWatch {
            kind: TitleKind::Manga,
            title_id,
            content_id: None,
        }
    }

    pub fn anime(title_id: u64) -> CommentWatch {
        CommentWatch {
            kind: TitleKind::Anime,
            title_id,
            content_id: None,
        }
    }

    /// Глава манги.
    pub fn chapter(title_id: u64, chapter_id: u64) -> CommentWatch {
        CommentWatch {
            content_id: Some(chapter_id),
            ..CommentWatch::manga(title_id)
        }
    }

    pub fn episode(title_id: u64, episode_id: u64) -> CommentWatch {
        CommentWatch {
            content_id: Some(episode_id),
            ..CommentWatch::anime(title_id)
        }
    }

    /// Параметры GET: `content_id` без значения не передаётся, как у сайта.
    fn query(self) -> Vec<(&'static str, String)> {
        let mut query = vec![
            ("source_type", self.kind.as_str().to_string()),
            ("source_id", self.title_id.to_string()),
        ];
        if let Some(id) = self.content_id {
            query.push(("content_id", id.to_string()));
        }
        query
    }

    /// Тело POST: тот же объект, `content_id` у тайтла – `null`.
    fn body(self) -> Value {
        json!({
            "source_type": self.kind.as_str(),
            "source_id": self.title_id,
            "content_id": self.content_id,
        })
    }
}

/// Состояние подписки на новые комментарии.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentNotify {
    #[serde(default, deserialize_with = "bool_lenient")]
    pub notify: bool,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub content_id: Option<u64>,
}

impl Client {
    /// Подписан ли вошедший на новые комментарии к объекту. Ответ без записи клиент
    /// считает отсутствием подписки; так ли отвечает сервер, не проверено.
    pub async fn comment_subscription(&self, watch: CommentWatch) -> Result<CommentNotify> {
        let path = "comments/subscription";
        let bytes = self
            .send_raw::<_, ()>(Site::MangaLib, Method::GET, path, &watch.query(), None)
            .await?
            .bytes()
            .await?;
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(CommentNotify::default());
        };
        Ok(record_of(&value, path)?.unwrap_or_default())
    }

    /// Переключает подписку на новые комментарии и возвращает новое состояние.
    /// `None` – запрос прошёл, но состояния в ответе нет: его перечитывают
    /// `comment_subscription`, а не переключают ещё раз. Сайт показывает кнопку
    /// только тем, у кого есть права на объект (автор, команда, модератор).
    pub async fn toggle_comment_subscription(&self, watch: CommentWatch) -> Result<Option<CommentNotify>> {
        let record: Option<Value> = self
            .send_record(
                Site::MangaLib,
                Method::POST,
                "comments/subscription",
                Some(&watch.body()),
            )
            .await?;
        // Запись без `notify` разобралась бы как «не подписан» и выдала бы
        // догадку за новое состояние.
        match record {
            Some(record) if record.get("notify").is_some() => {
                CommentNotify::deserialize(&record)
                    .map(Some)
                    .map_err(|source| Error::Decode {
                        context: "comments/subscription".into(),
                        source,
                    })
            }
            _ => Ok(None),
        }
    }

    /// Страница комментариев. Без входа свой голос не приходит.
    pub async fn comments(&self, target: Target, sort: CommentSort, page: u32) -> Result<CommentPage> {
        let mut query: Vec<(&str, String)> = target.query();
        query.extend(sort.params().into_iter().map(|(k, v)| (k, v.to_string())));
        query.push(("page", page.max(1).to_string()));
        let raw: RawPage = self.get_json(Site::MangaLib, "comments", &query).await?;
        Ok(CommentPage {
            roots: raw.data.root,
            replies: raw.data.replies,
            has_next: crate::listing::has_next(&raw.links, &raw.meta),
        })
    }

    /// Ветка целиком по id любого её комментария: для ссылок на комментарий.
    pub async fn comment_thread(&self, target: Target, comment_id: u64, sort: CommentSort) -> Result<CommentPage> {
        let mut query: Vec<(&str, String)> = target.query();
        query.extend(sort.params().into_iter().map(|(k, v)| (k, v.to_string())));
        let raw: RawPage = self
            .get_json(Site::MangaLib, &format!("comments/{comment_id}"), &query)
            .await?;
        Ok(CommentPage {
            roots: raw.data.root,
            replies: raw.data.replies,
            has_next: false,
        })
    }

    /// Закреплённые комментарии: показываются над списком.
    pub async fn sticky_comments(&self, target: Target) -> Result<Vec<Comment>> {
        let query = [
            ("post_type", target.post_type().to_string()),
            ("post_id", target.post_id().to_string()),
        ];
        self.get_data(Site::MangaLib, "comments/sticky", &query).await
    }

    /// Пишет комментарий или ответ. Возвращает его в том виде, в каком его
    /// сохранил сервер.
    pub async fn post_comment(&self, new: &NewComment) -> Result<Comment> {
        let mut doc = new.text.to_prosemirror();
        let content = doc["content"].as_array_mut().expect("документ со списком блоков");
        if !new.images.is_empty() {
            let images: Vec<Value> = new.images.iter().map(|i| json!({"image": i.name})).collect();
            content.push(json!({"type": "image", "attrs": {"description": "", "images": images}}));
        }
        if let Some(s) = &new.sticker {
            content.push(json!({"type": "sticker", "attrs": {"stickers": [{
                "sticker_pack_id": s.sticker_pack_id, "extension": s.extension, "name": s.name, "id": s.id
            }]}}));
        }
        let (target, parent, root, level) = match &new.reply_to {
            Some(p) => (
                p.target().unwrap_or(new.target),
                Some(p.id),
                Some(p.root_id.unwrap_or(p.id)),
                p.comment_level + 1,
            ),
            None => (new.target, None, None, 0),
        };
        let mut body = json!({
            "comment": doc,
            "attachments": new.images,
            "post_type": target.post_type(),
            "post_id": target.post_id(),
            "parent_comment": parent,
            "root_id": root,
            "comment_level": level,
        });
        if let Target::Chapter { page, .. } = target {
            body["post_page"] = page.unwrap_or(1).into();
        }
        self.send_data(Site::MangaLib, Method::POST, "comments", &(), Some(&body))
            .await
    }

    /// Правит свой комментарий. Сайт разрешает это в течение двух дней.
    pub async fn edit_comment(&self, comment_id: u64, text: &Doc, images: &[Uploaded]) -> Result<Comment> {
        let body = json!({"comment": text.to_prosemirror(), "attachments": images});
        self.send_data(
            Site::MangaLib,
            Method::PUT,
            &format!("comments/{comment_id}"),
            &(),
            Some(&body),
        )
        .await
    }

    pub async fn delete_comment(&self, comment_id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("comments/{comment_id}"), None)
            .await
    }

    /// Голос за комментарий. Повторный тот же голос снимает его.
    pub async fn vote_comment(&self, comment_id: u64, vote: Vote) -> Result<()> {
        self.vote("comments", comment_id, vote).await
    }

    /// Голос за что угодно, что поддерживает голоса: `comments`, `reviews`,
    /// `collection`, `similar`.
    pub async fn vote(&self, kind: &str, id: u64, vote: Vote) -> Result<()> {
        let body = json!({"vote": vote.value()});
        self.send_ok(Site::MangaLib, Method::POST, &format!("{kind}/{id}/vote"), Some(&body))
            .await
    }

    /// Причины жалоб на комментарии.
    pub async fn report_types(&self) -> Result<Vec<ReportType>> {
        self.report_reasons(COMMENT_NAMESPACE).await
    }

    /// Жалоба на комментарий. `reason` – `ReportType::name`.
    pub async fn report_comment(&self, comment_id: u64, reason: &str, note: &str) -> Result<()> {
        let body = json!({
            "reason": reason,
            "metadata": {"comment": note},
            "reportable_id": comment_id,
            "reportable_type": "comment",
            "namespaces": COMMENT_NAMESPACE,
        });
        self.send_ok(
            Site::MangaLib,
            Method::POST,
            &format!("comments/{comment_id}/report"),
            Some(&body),
        )
        .await
    }

    /// Наборы стикеров, только для вошедшего. Пользоваться ими в комментариях может
    /// только премиум.
    pub async fn sticker_packs(&self) -> Result<Vec<StickerPack>> {
        self.get_data(Site::MangaLib, "sticker-packs", &()).await
    }

    /// Загружает картинки для комментария: jpeg, png, gif. Только премиум.
    pub async fn upload_comment_images(&self, files: Vec<(String, Vec<u8>)>) -> Result<Vec<Uploaded>> {
        let response = self
            .send_api(Auth::User, |http, base| {
                let mut form = reqwest::multipart::Form::new();
                for (name, bytes) in &files {
                    let mime = match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
                        Some("png") => "image/png",
                        Some("gif") => "image/gif",
                        _ => "image/jpeg",
                    };
                    let part = reqwest::multipart::Part::bytes(bytes.clone())
                        .file_name(name.clone())
                        .mime_str(mime)
                        .expect("известный тип файла");
                    form = form.part("content[]", part);
                }
                http.post(join_url(base, "upload/image/comment"))
                    .headers(Self::site_headers(Site::MangaLib))
                    .multipart(form)
            })
            .await?;
        let bytes = response.bytes().await?;
        let body: crate::models::Data<Vec<Uploaded>> =
            serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
                context: "upload/image/comment".into(),
                source,
            })?;
        Ok(body.data)
    }

    /// Комментарии, которые оставил пользователь, по 20 на страницу.
    pub async fn user_comments(&self, user_id: u64, q: &UserCommentsQuery) -> Result<Page<Comment>> {
        let mut query: Vec<(&str, String)> = vec![
            ("page", q.page.max(1).to_string()),
            ("sort_by", if q.by_votes { "votes" } else { "id" }.into()),
            ("sort_type", if q.ascending { "asc" } else { "desc" }.into()),
        ];
        if let Some(t) = &q.post_type {
            query.push(("post_type", t.clone()));
        }
        if q.with_images {
            query.push(("has_images", "1".into()));
        }
        if q.text.trim().chars().count() >= 2 {
            query.push(("q", q.text.trim().to_string()));
        }
        self.get_page(Site::MangaLib, &format!("user/{user_id}/comments"), &query)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = Target::Chapter {
            chapter_id: 5,
            page: Some(3),
        };
        assert_eq!(
            t.query(),
            vec![
                ("post_type", "chapter".to_string()),
                ("post_id", "5".to_string()),
                ("post_page", "3".to_string())
            ]
        );
        assert_eq!(Target::from_relation("episodes", 9, None), Some(Target::Episode(9)));
        assert_eq!(Target::from_relation("chapter", 5, Some(3)), Some(t));
    }

    #[test]
    fn comment_parses() {
        let c: Comment = serde_json::from_value(json!({
            "id": 300000001, "root_id": null, "parent_comment": null, "comment_level": 0,
            "post_page": 1, "comment": "<p>текст</p>",
            "created_at": "2026-10-08T08:42:40.000000Z", "created_at_ts": 1791448960000_u64,
            "relation_type": "chapter", "relation_id": 223269,
            "user": {"username": "user1", "id": 1000001, "avatar": {"filename": "a1.jpg", "url": "https://c/a.jpg"},
                     "login_streak": {"last_login_at": "2026-10-14 00:12:43", "login_streak": 50}, "premium": {"enabled": false}},
            "votes": {"up": 4, "down": 1, "user": 1}
        }))
        .unwrap();
        assert!(c.is_root());
        assert_eq!(c.votes.score(), 3);
        assert_eq!(c.votes.user, Some(Vote::Up));
        assert_eq!(c.text().plain_text(), "текст");
        assert_eq!(
            c.target(),
            Some(Target::Chapter {
                chapter_id: 223269,
                page: Some(1)
            })
        );
    }
}
