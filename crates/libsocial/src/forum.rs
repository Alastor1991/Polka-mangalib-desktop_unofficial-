//! Форум сети: разделы, список тем с фильтрами, закреплённые темы, тема целиком,
//! новая тема и правка, подписка на тему, лайк, картинки в тексте темы. Форум общий
//! для всех сайтов сети, поэтому запросы идут от MangaLib.
//!
//! Список тем приходит плоским пагинатором Laravel, его разбирает `listing::Page`.
//! Текст темы сервер хранит в трёх видах: новые темы – документом ProseMirror (в
//! списке он упакован в строку JSON), старые – документом Quill (`{"ops": […]}`),
//! совсем старые бывают строкой HTML. `discussion_doc` разбирает все три.
//! Комментарии к теме – общие комментарии с `comments::Target::Discussion`, жалоба
//! на тему – `reports::ReportTarget::Discussion`. Действия модератора с темой
//! (`/forum/discussion/{id}/action`) клиент не делает.

use reqwest::Method;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::client::{Client, record_of};
use crate::comments::{Uploaded, image_node};
use crate::error::{Error, Result};
use crate::listing::{Page, bool_lenient};
use crate::models::Cover;
use crate::rich::Doc;
use crate::serde_util::{
    lenient_vec, null_default, opt_lenient, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient,
};
use crate::site::Site;
use crate::users::UserRef;

/// Раздел форума. Его же присылает главная в темах форума (`feeds::ForumTopic`), там
/// обычно только `id`, `name` и `color`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumCategory {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub color: Option<String>,
    /// Имя значка Font Awesome.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub icon: Option<String>,
    /// Место в списке разделов.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub order: u64,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub parent_id: Option<u64>,
}

impl ForumCategory {
    /// Раздел верхнего уровня. Сайт показывает в боковой панели только такие:
    /// `parent_id` у них `null` или 0.
    pub fn is_top_level(&self) -> bool {
        matches!(self.parent_id, None | Some(0))
    }
}

/// Лайки темы.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionVotes {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub up: u64,
    /// Свой лайк: сервер присылает `votes.user` равным 1 или `null`. В списке тем
    /// этого поля нет.
    #[serde(default, rename = "user", deserialize_with = "bool_lenient")]
    pub liked: bool,
}

/// Тема в списке и в закреплённых.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discussion {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub title: String,
    /// Раздел темы. Его название берут из `forum_categories`: `category` в ответе
    /// приходит пустым объектом.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub chatter_category_id: Option<u64>,
    #[serde(default, deserialize_with = "known_category")]
    pub category: Option<ForumCategory>,
    #[serde(default, deserialize_with = "opt_lenient")]
    pub user: Option<UserRef>,
    /// Текст как пришёл: в списке строка JSON, в закреплённых `null`. Разобранный
    /// текст – `body_doc`.
    #[serde(default)]
    pub body: Value,
    /// Число ответов.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub answered: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: DiscussionVotes,
    /// Число лайков, которое присылает тема целиком рядом с `votes.up`.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub likes: Option<u64>,
    /// Уровень закрепления: 0 – не закреплена, у модератора на выбор 1–3.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub sticky: u64,
    /// Закрыта для ответов.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub locked: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub yaoi: bool,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub last_reply_at: Option<String>,
}

impl Discussion {
    /// Текст темы из списка. Картинок в нём нет: вложения, на которые ссылается
    /// документ, присылает только тема целиком (`discussion`).
    pub fn body_doc(&self) -> Doc {
        discussion_doc(&self.body, &[], &|p| p.to_string())
    }

    /// Число лайков. Сайт берёт `votes.up`, а без него `likes`.
    pub fn like_count(&self) -> u64 {
        match self.likes {
            Some(likes) if self.votes.up == 0 => likes,
            _ => self.votes.up,
        }
    }
}

/// Пустой объект `category` (так его присылает список) – не раздел.
fn known_category<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<ForumCategory>, D::Error> {
    Ok(opt_lenient::<D, ForumCategory>(d)?.filter(|c| c.id != 0))
}

/// К чему привязана тема: тайтл или команда. Тот же вид у фильтра списка тем на
/// странице тайтла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiscussionSource {
    Manga(u64),
    Anime(u64),
    Ranobe(u64),
    Team(u64),
}

impl DiscussionSource {
    /// `source_type` запроса.
    pub fn source_type(self) -> &'static str {
        match self {
            DiscussionSource::Manga(_) => "manga",
            DiscussionSource::Anime(_) => "anime",
            DiscussionSource::Ranobe(_) => "ranobe",
            DiscussionSource::Team(_) => "team",
        }
    }

    pub fn id(self) -> u64 {
        match self {
            DiscussionSource::Manga(id)
            | DiscussionSource::Anime(id)
            | DiscussionSource::Ranobe(id)
            | DiscussionSource::Team(id) => id,
        }
    }

    /// Из `source_type` и `source_id` ответа или `model` связи. Ранобэ сервер
    /// называет `book`, сайт переводит его в `ranobe`.
    pub fn from_parts(kind: &str, id: u64) -> Option<DiscussionSource> {
        Some(match kind {
            "manga" => DiscussionSource::Manga(id),
            "anime" => DiscussionSource::Anime(id),
            "ranobe" | "book" => DiscussionSource::Ranobe(id),
            "team" => DiscussionSource::Team(id),
            _ => return None,
        })
    }
}

/// Тайтл или команда, к которым привязана тема.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionRelation {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// `manga`, `anime`, `book` (ранобэ) или `team`.
    #[serde(default, deserialize_with = "string_lenient")]
    pub model: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub rus_name: Option<String>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub alt_name: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "opt_lenient")]
    pub cover: Option<Cover>,
}

impl DiscussionRelation {
    pub fn source(&self) -> Option<DiscussionSource> {
        DiscussionSource::from_parts(&self.model, self.id)
    }
}

/// Тема целиком.
#[derive(Debug, Clone)]
pub struct DiscussionDetails {
    pub item: Discussion,
    /// Текст темы с полными адресами картинок.
    pub content: Doc,
    /// Текст как пришёл: `content` ответа, а если его нет, `post.body`.
    pub content_raw: Value,
    /// Картинки текста. Чтобы они пережили правку темы, их передают обратно в
    /// `NewDiscussion::images`.
    pub attachments: Vec<Uploaded>,
    /// Id первого сообщения темы.
    pub post_id: Option<u64>,
    pub relation: Option<DiscussionRelation>,
    pub source: Option<DiscussionSource>,
    /// Подписан ли пользователь на тему.
    pub subscribed: bool,
    /// Может ли пользователь закрыть тему для ответов (автор или модератор).
    pub can_toggle_lock: bool,
}

#[derive(Deserialize)]
struct RawDetails {
    #[serde(flatten)]
    item: Discussion,
    #[serde(default)]
    content: Value,
    #[serde(default, deserialize_with = "opt_lenient")]
    post: Option<RawPost>,
    #[serde(default, deserialize_with = "lenient_vec")]
    attachments: Vec<Uploaded>,
    #[serde(default, deserialize_with = "opt_lenient")]
    relation: Option<DiscussionRelation>,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    source_type: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    source_id: Option<u64>,
    #[serde(default, deserialize_with = "bool_lenient")]
    notification: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    can_toggle_lock: bool,
}

#[derive(Deserialize)]
struct RawPost {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    id: Option<u64>,
    #[serde(default)]
    body: Value,
    #[serde(default, deserialize_with = "lenient_vec")]
    attachments: Vec<Uploaded>,
}

/// Порядок списка тем.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiscussionSort {
    #[default]
    Newest,
    Popular,
    /// По дате обновления.
    Updates,
}

impl DiscussionSort {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscussionSort::Newest => "newest",
            DiscussionSort::Popular => "popular",
            DiscussionSort::Updates => "updates",
        }
    }
}

/// Какие темы показывать: параметр `category`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForumSection {
    #[default]
    All,
    Category(u64),
    /// Свои темы, только с входом.
    Mine,
}

impl ForumSection {
    fn param(self) -> String {
        match self {
            ForumSection::All => "all".into(),
            ForumSection::Category(id) => id.to_string(),
            ForumSection::Mine => "user".into(),
        }
    }
}

/// Фильтр списка тем.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscussionQuery {
    /// Номер страницы с единицы, по 10 тем.
    pub page: u32,
    pub sort: DiscussionSort,
    pub section: ForumSection,
    /// Темы, на которые подписан пользователь. Только с входом, раздел тогда
    /// не учитывается.
    pub subscribed: bool,
    /// Темы авторов, на которых подписан пользователь. Только с входом, раздел
    /// тогда не учитывается.
    pub followed_authors: bool,
    /// Поиск по названию.
    pub title: String,
    /// Темы одного пользователя.
    pub user_id: Option<u64>,
    /// Темы тайтла или команды, как на странице тайтла. С ним остальные фильтры,
    /// кроме порядка и страницы, не уходят.
    pub source: Option<DiscussionSource>,
}

impl DiscussionQuery {
    /// Нужен ли вход: свои темы и подписки сайт гостю не предлагает.
    fn needs_login(&self) -> bool {
        self.source.is_none() && (self.section == ForumSection::Mine || self.subscribed || self.followed_authors)
    }

    /// `category` с учётом фильтров подписок: при них сайт шлёт `all`.
    fn category(&self) -> String {
        if self.subscribed || self.followed_authors {
            ForumSection::All.param()
        } else {
            self.section.param()
        }
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        let page = ("page", self.page.max(1).to_string());
        let sort = ("sort", self.sort.as_str().to_string());
        if let Some(source) = self.source {
            return vec![
                page,
                ("category", ForumSection::All.param()),
                ("source_type", source.source_type().into()),
                ("source_id", source.id().to_string()),
                sort,
            ];
        }
        let mut query = vec![
            page,
            sort,
            ("subscription", u8::from(self.subscribed).to_string()),
            ("author_subscription", u8::from(self.followed_authors).to_string()),
            ("category", self.category()),
        ];
        let title = self.title.trim();
        if !title.is_empty() {
            query.push(("title", title.to_string()));
        }
        if let Some(user_id) = self.user_id {
            query.push(("user_id", user_id.to_string()));
        }
        query
    }
}

/// Новая тема или правка.
#[derive(Debug, Clone)]
pub struct NewDiscussion {
    pub title: String,
    /// Раздел из `forum_categories`.
    pub category_id: u64,
    pub yaoi: bool,
    /// Тайтл или команда, к которым привязать тему.
    pub source: Option<DiscussionSource>,
    pub text: Doc,
    /// Картинки из `upload_discussion_images` (или `DiscussionDetails::attachments`
    /// при правке), в порядке показа.
    pub images: Vec<Uploaded>,
}

impl NewDiscussion {
    /// Тело запроса. Без названия или раздела сайт кнопку не активирует, и клиент
    /// отказывает до запроса.
    fn body(&self) -> Result<Value> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err(Error::Other("У темы нет названия".into()));
        }
        if self.category_id == 0 {
            return Err(Error::Other("Не выбран раздел форума".into()));
        }
        let mut doc = self.text.to_prosemirror();
        let content = doc["content"].as_array_mut().expect("документ со списком блоков");
        content.extend(image_node(&self.images));
        Ok(json!({
            "title": title,
            "chatter_category_id": self.category_id,
            "yaoi": self.yaoi,
            "source_type": self.source.map(DiscussionSource::source_type),
            "source_id": self.source.map(DiscussionSource::id),
            "body": doc,
            "attachments": self.images,
        }))
    }
}

/// Лайки темы после лайка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LikeReply {
    pub up: u64,
    /// Свой лайк. `None`, если сервер прислал только число: сайт тогда сам
    /// переключает свой прежний признак.
    pub liked: Option<bool>,
}

#[derive(Deserialize)]
struct RawVoteReply {
    #[serde(default, deserialize_with = "opt_lenient")]
    parent: Option<RawVoteParent>,
}

#[derive(Deserialize)]
struct RawVoteParent {
    #[serde(default, deserialize_with = "opt_lenient")]
    votes: Option<DiscussionVotes>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    likes: Option<u64>,
}

#[derive(Deserialize)]
struct RawCreated {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    id: Option<u64>,
    #[serde(default, deserialize_with = "opt_lenient")]
    discussion: Option<RawCreatedId>,
}

#[derive(Deserialize)]
struct RawCreatedId {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    id: Option<u64>,
}

impl Client {
    /// Разделы форума: верхнего уровня и вложенные (`ForumCategory::is_top_level`).
    pub async fn forum_categories(&self) -> Result<Vec<ForumCategory>> {
        // Ответ – голый массив, сайт принимает и обёртку `data`.
        let value: Value = self.get_json(Site::MangaLib, "forum/categories", &()).await?;
        Ok(list_of(value))
    }

    /// Страница списка тем, по 10.
    pub async fn discussions(&self, query: &DiscussionQuery) -> Result<Page<Discussion>> {
        if query.needs_login() && !self.is_logged_in().await {
            return Err(Error::Unauthorized);
        }
        self.get_page(Site::MangaLib, "forum/discussion", &query.params()).await
    }

    /// Закреплённые темы над списком. Учитывается только раздел запроса. Для своих
    /// тем, фильтров подписок и списка тайтла сайт закреплённых не спрашивает, и
    /// клиент тогда отдаёт пустой список без запроса.
    pub async fn sticky_discussions(&self, query: &DiscussionQuery) -> Result<Vec<Discussion>> {
        if query.source.is_some() || query.needs_login() {
            return Ok(Vec::new());
        }
        let params = [
            ("category", query.section.param()),
            ("subscription", "0".into()),
            ("author_subscription", "0".into()),
        ];
        let value: Value = self
            .get_json(Site::MangaLib, "forum/discussion/sticky", &params)
            .await?;
        Ok(list_of(value))
    }

    /// Тема целиком. Отказ сервер может прислать с кодом 200 и сообщением в
    /// `data.toast`: клиент отдаёт его как `Error::NotFound` или `Error::Other` с
    /// текстом сервера.
    pub async fn discussion(&self, id: u64) -> Result<DiscussionDetails> {
        let path = format!("forum/discussion/{id}");
        let value: Value = self.get_json(Site::MangaLib, &path, &()).await?;
        if let Some(error) = toast_error(&value) {
            return Err(error);
        }
        // Сайт ищет тему и в `discussion`, и прямо в ответе, с обёрткой `data` и без.
        let record = value.get("data").unwrap_or(&value);
        let record = record.get("discussion").unwrap_or(record);
        if record.get("id").is_none() {
            return Err(Error::NotFound);
        }
        let raw = RawDetails::deserialize(record).map_err(|source| Error::Decode { context: path, source })?;
        let (post_id, post_body, post_attachments) = match raw.post {
            Some(post) => (post.id, post.body, post.attachments),
            None => (None, Value::Null, Vec::new()),
        };
        let content_raw = if raw.content.is_null() { post_body } else { raw.content };
        let attachments = if raw.attachments.is_empty() {
            post_attachments
        } else {
            raw.attachments
        };
        let content = discussion_doc(&content_raw, &attachments, &|p| self.upload_url(p));
        let source = raw
            .source_type
            .as_deref()
            .zip(raw.source_id)
            .and_then(|(kind, id)| DiscussionSource::from_parts(kind, id));
        Ok(DiscussionDetails {
            item: raw.item,
            content,
            content_raw,
            attachments,
            post_id,
            relation: raw.relation,
            source,
            subscribed: raw.notification,
            can_toggle_lock: raw.can_toggle_lock,
        })
    }

    /// Создаёт тему. Возвращает её id, если сервер его прислал; иначе тему ищут в
    /// своих темах (`ForumSection::Mine`). Отказ с кодом 200 и `toast` без id даёт
    /// ошибку с текстом сервера.
    pub async fn create_discussion(&self, new: &NewDiscussion) -> Result<Option<u64>> {
        let body = new.body()?;
        let path = "forum/discussion";
        let Some(value) = self.write_json(Method::POST, path, &body).await? else {
            return Ok(None);
        };
        let id = record_of::<RawCreated>(&value, path)
            .ok()
            .flatten()
            .and_then(|c| c.discussion.and_then(|d| d.id).or(c.id));
        refuse_by_toast(id, &value)
    }

    /// Правит свою тему тем же телом, что у новой. Отказ с кодом 200 и `toast` даёт
    /// ошибку с текстом сервера.
    pub async fn edit_discussion(&self, id: u64, new: &NewDiscussion) -> Result<()> {
        let body = new.body()?;
        let value = self
            .write_json(Method::PUT, &format!("forum/discussion/{id}"), &body)
            .await?;
        match value.as_ref().and_then(toast_error) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Переключает подписку на тему. Возвращает новое состояние, если сервер его
    /// прислал; иначе его перечитывают через `discussion`.
    pub async fn toggle_discussion_subscription(&self, id: u64) -> Result<Option<bool>> {
        let path = format!("forum/discussion/{id}/notification");
        let Some(value) = self.write_json(Method::POST, &path, &json!({})).await? else {
            return Ok(None);
        };
        // Ответ – булево значение в `data` или голым значением.
        let state = match value.get("data").unwrap_or(&value) {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => n.as_f64().map(|n| n != 0.0),
            Value::String(s) => match s.trim() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            },
            _ => None,
        };
        refuse_by_toast(state, &value)
    }

    /// Лайк темы. Снимает ли повторный лайк прежний, по скриптам сайта не видно.
    /// Отказ сервер может прислать с кодом 200 и сообщением в `data.toast`, клиент
    /// отдаёт его как `Error::Other` с текстом сервера.
    pub async fn like_discussion(&self, id: u64) -> Result<Option<LikeReply>> {
        let path = format!("discussion/{id}/vote");
        let Some(value) = self.write_json(Method::POST, &path, &json!({"vote": 1})).await? else {
            return Ok(None);
        };
        let parent = record_of::<RawVoteReply>(&value, &path)
            .ok()
            .flatten()
            .and_then(|r| r.parent);
        let reply = parent.and_then(|p| match (p.votes, p.likes) {
            (Some(votes), _) => Some(LikeReply {
                up: votes.up,
                liked: Some(votes.liked),
            }),
            (None, Some(likes)) => Some(LikeReply { up: likes, liked: None }),
            (None, None) => None,
        });
        refuse_by_toast(reply, &value)
    }

    /// Запрос на запись с телом JSON. Ответ, который не читается как JSON, даёт
    /// `None`: запись на сервере уже сделана, и ошибка разбора заставила бы
    /// вызывающего её повторить.
    async fn write_json(&self, method: Method, path: &str, body: &Value) -> Result<Option<Value>> {
        let bytes = self
            .send_raw(Site::MangaLib, method, path, &(), Some(body))
            .await?
            .bytes()
            .await?;
        Ok(serde_json::from_slice(&bytes).ok())
    }

    /// Загружает картинки для текста темы: jpeg, png, gif. Путь взят из редактора
    /// сайта (`/upload/image`, у комментариев `/upload/image/comment`), вживую не
    /// проверен.
    pub async fn upload_discussion_images(&self, files: Vec<(String, Vec<u8>)>) -> Result<Vec<Uploaded>> {
        self.upload_images("upload/image", files).await
    }
}

/// Список из голого массива или из `data`. Элемент, который не разобрался,
/// выбрасывается.
fn list_of<T: serde::de::DeserializeOwned>(value: Value) -> Vec<T> {
    let items = match value {
        Value::Object(mut map) => map.remove("data").unwrap_or(Value::Null),
        other => other,
    };
    match items {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Результат записи или отказ из `toast`. Отказ проверяется только тогда, когда
/// результата в ответе нет: если сервер прислал и запись, и сообщение, запись
/// сделана, и повторять её нельзя.
fn refuse_by_toast<T>(result: Option<T>, value: &Value) -> Result<Option<T>> {
    match (result, toast_error(value)) {
        (Some(result), _) => Ok(Some(result)),
        (None, Some(error)) => Err(error),
        (None, None) => Ok(None),
    }
}

/// Отказ в `toast.message` (с обёрткой `data` или без). Сайт считает «not found» в
/// тексте отсутствием темы, остальное – отказом в доступе.
fn toast_error(value: &Value) -> Option<Error> {
    let toast = value
        .get("data")
        .and_then(|d| d.get("toast"))
        .or_else(|| value.get("toast"))
        .filter(|t| t.is_object())?;
    let message = toast.get("message").and_then(Value::as_str).unwrap_or_default();
    Some(if message.to_lowercase().contains("not found") {
        Error::NotFound
    } else if message.trim().is_empty() {
        Error::Forbidden
    } else {
        Error::Other(message.to_string())
    })
}

/// Текст темы в любом из видов, в которых его хранит сервер: ProseMirror объектом
/// или строкой JSON, Quill (`ops`) или строка HTML. Картинки ProseMirror ссылаются
/// на вложения по имени, им подставляются адреса из `attachments`; картинка без
/// вложения выбрасывается.
fn discussion_doc(raw: &Value, attachments: &[Uploaded], absolute: &dyn Fn(&str) -> String) -> Doc {
    match raw {
        Value::Null => Doc::default(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => discussion_doc(&v, attachments, absolute),
            // Строку не-JSON сайт вставляет как HTML.
            _ => Doc::from_html(s),
        },
        v if v.get("ops").is_some() => Doc::from_quill(v),
        v => {
            let mut v = v.clone();
            resolve_images(&mut v, attachments, absolute);
            Doc::from_prosemirror(&v)
        }
    }
}

/// Непустой адрес вложения.
fn present(url: &Option<String>) -> Option<&str> {
    url.as_deref().filter(|u| !u.is_empty())
}

/// Подставляет картинкам документа адреса вложений.
fn resolve_images(node: &mut Value, attachments: &[Uploaded], absolute: &dyn Fn(&str) -> String) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(|n| resolve_images(n, attachments, absolute)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && let Some(images) = map
                    .get_mut("attrs")
                    .and_then(|a| a.get_mut("images"))
                    .and_then(Value::as_array_mut)
            {
                let resolved: Vec<Value> = images
                    .iter()
                    .filter_map(|entry| {
                        let name = entry.get("image").and_then(Value::as_str)?;
                        let att = attachments
                            .iter()
                            .find(|a| a.name == name || a.filename.as_deref() == Some(name))?;
                        let shown = present(&att.preview_url)
                            .or(present(&att.url))
                            .or(present(&att.original_url))?;
                        let original = present(&att.original_url).or(present(&att.url));
                        Some(json!({
                            "url": absolute(shown),
                            "originalUrl": original.map(absolute),
                            "width": att.width,
                            "height": att.height,
                        }))
                    })
                    .collect();
                *images = resolved;
            }
            if let Some(children) = map.get_mut("content") {
                resolve_images(children, attachments, absolute);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::{Block, Inline};

    fn uploaded(name: &str) -> Uploaded {
        serde_json::from_value(json!({
            "name": name, "filename": format!("{name}.jpg"), "extension": "jpg",
            "url": format!("/uploads/forum/posts/1/{name}.jpg"),
            "previewUrl": format!("/uploads/forum/posts/1/{name}.jpg"),
            "originalUrl": format!("/uploads/forum/posts/1/{name}.jpg"),
            "width": "736", "height": 414
        }))
        .unwrap()
    }

    #[test]
    fn body_in_every_shape() {
        let abs = |p: &str| format!("https://api.example{p}");
        let pm = json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": [], "content": [{"type": "text", "text": "Текст"}]},
            {"type": "image", "attrs": {"description": "", "images": [{"image": "a1"}, {"image": "нет"}]}}
        ]});

        // Объект с вложениями: адрес картинки полный, неизвестное имя выброшено.
        let doc = discussion_doc(&pm, &[uploaded("a1")], &abs);
        assert_eq!(doc.plain_text(), "Текст");
        let Block::Images(pictures) = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks)
        };
        assert_eq!(pictures.len(), 1);
        assert_eq!(pictures[0].url, "https://api.example/uploads/forum/posts/1/a1.jpg");
        assert_eq!(pictures[0].width, Some(736));

        // Строка JSON из списка: без вложений картинка не показывается вовсе.
        let doc = discussion_doc(&Value::String(pm.to_string()), &[], &abs);
        assert_eq!(doc.blocks.len(), 1);

        // Quill объектом и строкой.
        let quill =
            json!({"ops": [{"insert": "Старая "}, {"insert": "тема", "attributes": {"bold": true}}, {"insert": "\n"}]});
        let doc = discussion_doc(&quill, &[], &abs);
        let Block::Paragraph(p) = &doc.blocks[0] else { panic!() };
        assert!(matches!(&p[1], Inline::Text { marks, .. } if marks.bold));
        assert_eq!(
            discussion_doc(&Value::String(quill.to_string()), &[], &abs).plain_text(),
            "Старая тема"
        );

        // HTML-строка и пустое тело.
        assert_eq!(discussion_doc(&json!("<p>html</p>"), &[], &abs).plain_text(), "html");
        assert!(discussion_doc(&Value::Null, &[], &abs).blocks.is_empty());
    }

    #[test]
    fn query_params_follow_site() {
        let q = DiscussionQuery::default();
        assert_eq!(
            q.params(),
            vec![
                ("page", "1".into()),
                ("sort", "newest".into()),
                ("subscription", "0".into()),
                ("author_subscription", "0".into()),
                ("category", "all".into()),
            ]
        );

        // Подписки отменяют раздел, поиск обрезается.
        let q = DiscussionQuery {
            page: 3,
            sort: DiscussionSort::Updates,
            section: ForumSection::Category(5),
            subscribed: true,
            title: "  ищу  ".into(),
            user_id: Some(9),
            ..Default::default()
        };
        let params = q.params();
        assert!(params.contains(&("category", "all".into())));
        assert!(params.contains(&("subscription", "1".into())));
        assert!(params.contains(&("title", "ищу".into())));
        assert!(params.contains(&("user_id", "9".into())));
        assert!(q.needs_login());

        // Темы тайтла: только тайтл, порядок и страница.
        let q = DiscussionQuery {
            source: Some(DiscussionSource::Ranobe(77)),
            sort: DiscussionSort::Popular,
            subscribed: true,
            ..Default::default()
        };
        assert_eq!(
            q.params(),
            vec![
                ("page", "1".into()),
                ("category", "all".into()),
                ("source_type", "ranobe".into()),
                ("source_id", "77".into()),
                ("sort", "popular".into()),
            ]
        );
        assert!(!q.needs_login());

        let mine = DiscussionQuery {
            section: ForumSection::Mine,
            ..Default::default()
        };
        assert!(mine.params().contains(&("category", "user".into())));
        assert!(mine.needs_login());
    }

    #[test]
    fn new_discussion_body() {
        let new = NewDiscussion {
            title: "  Название ".into(),
            category_id: 5,
            yaoi: false,
            source: Some(DiscussionSource::Manga(7965)),
            text: Doc::from_plain("Привет"),
            images: vec![uploaded("a1")],
        };
        let body = new.body().unwrap();
        assert_eq!(body["title"], "Название");
        assert_eq!(body["chatter_category_id"], 5);
        assert_eq!(body["source_type"], "manga");
        assert_eq!(body["source_id"], 7965);
        assert_eq!(body["body"]["content"][1]["attrs"]["images"][0]["image"], "a1");
        assert_eq!(body["attachments"][0]["name"], "a1");

        let unbound = NewDiscussion {
            source: None,
            images: Vec::new(),
            ..new.clone()
        };
        let body = unbound.body().unwrap();
        assert!(body["source_type"].is_null() && body["source_id"].is_null());
        assert_eq!(body["body"]["content"].as_array().unwrap().len(), 1);

        assert!(matches!(
            NewDiscussion {
                title: " ".into(),
                ..new.clone()
            }
            .body(),
            Err(Error::Other(_))
        ));
        assert!(matches!(
            NewDiscussion { category_id: 0, ..new }.body(),
            Err(Error::Other(_))
        ));
    }

    #[test]
    fn toasts() {
        assert!(matches!(
            toast_error(&json!({"data": {"toast": {"message": "Discussion not found"}}})),
            Some(Error::NotFound)
        ));
        assert!(matches!(
            toast_error(&json!({"toast": {"message": "Нет доступа"}})),
            Some(Error::Other(m)) if m == "Нет доступа"
        ));
        assert!(toast_error(&json!({"discussion": {"id": 1}})).is_none());
    }

    #[test]
    fn source_names() {
        assert_eq!(
            DiscussionSource::from_parts("book", 3),
            Some(DiscussionSource::Ranobe(3))
        );
        assert_eq!(
            DiscussionSource::from_parts("team", 4).map(DiscussionSource::source_type),
            Some("team")
        );
        assert_eq!(DiscussionSource::from_parts("user", 1), None);
    }
}
