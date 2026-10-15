//! Друзья, игнор-лист и личные сообщения. Websocket и мини-чат сюда не входят.
//!
//! Всё требует входа. Главные ловушки сервиса:
//! - заявки принимаются, отклоняются и удаляются по id записи дружбы
//!   (`Friendship::id`), а не по id пользователя;
//! - списки друзей и сообщений не отдают `meta.last_page`, листать надо по `has_next`,
//!   а короткие ответы приходят без `meta` и `links` вовсе;
//! - `GET /ignore` без `sourceable_type` отвечает 422, а игнор от имени команды или
//!   автора требует комментарий;
//! - диалога с человеком может не быть, тогда `find` отвечает 404, а архивные диалоги
//!   поиск тоже находит;
//! - текст сообщения простой, но сайт вставляет его как HTML: при показе его надо
//!   экранировать.

use std::collections::HashMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{Auth, Client, join_url};
use crate::error::{Error, Result};
use crate::listing::{Page, bool_lenient};
use crate::serde_util::{null_default, opt_u64_lenient, u64_lenient};
use crate::site::Site;
use crate::users::UserRef;

/// Самое длинное сообщение, которое принимает сайт.
pub const MESSAGE_MAX_CHARS: usize = 2000;

/// Статус в ответе на заявку, одиночный или массовый: принять.
const FRIEND_ACCEPT: u8 = 1;
/// Отклонить входящую заявку или отозвать свою.
const FRIEND_DECLINE: u8 = 2;

/// Флаги дружбы между вошедшим пользователем и другим.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendshipStatus {
    /// Я отправил заявку, ответа ещё нет.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_requested: bool,
    /// Мне прислали заявку.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_awaiting_confirmation: bool,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_friend: bool,
    /// Есть в скриптах сайта рядом с остальными флагами. Кто кого заблокировал,
    /// не выяснено; в ответах, которые видел клиент, поля не было.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_blocked: bool,
}

/// Что кнопка профиля предлагает сделать при таких флагах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendshipAction {
    /// Дружбы нет: можно отправить заявку.
    Request,
    /// Мне прислали заявку: можно принять.
    Accept,
    /// Я отправил заявку: можно отозвать.
    Revoke,
    /// Уже друзья: можно удалить из друзей.
    Remove,
}

impl FriendshipStatus {
    /// `is_blocked` не учитывается: что он значит, не выяснено.
    pub fn available_action(self) -> FriendshipAction {
        if self.is_friend {
            FriendshipAction::Remove
        } else if self.is_awaiting_confirmation {
            FriendshipAction::Accept
        } else if self.is_requested {
            FriendshipAction::Revoke
        } else {
            FriendshipAction::Request
        }
    }
}

/// Запись дружбы или заявки. Когда дружбы нет, `id` и `user` равны `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Friendship {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub id: Option<u64>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub status: FriendshipStatus,
}

/// Какой список дружбы читать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendList {
    Friends,
    /// Входящие заявки. Только в собственном профиле.
    Incoming,
    /// Отправленные заявки. Только в собственном профиле.
    Outgoing,
}

/// Массовое действие над входящими заявками.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendBulk {
    AcceptAll,
    DenyAll,
}

impl FriendBulk {
    /// Статус дружбы в запросе: 1 принять, 2 отклонить.
    fn status(self) -> u8 {
        match self {
            FriendBulk::AcceptAll => FRIEND_ACCEPT,
            FriendBulk::DenyAll => FRIEND_DECLINE,
        }
    }
}

/// Где действует игнор.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreSource {
    /// Личный игнор-лист; значение – id самого вошедшего пользователя.
    User(u64),
    /// От имени команды, нужен комментарий.
    Team(u64),
    /// От имени автора, нужен комментарий.
    People(u64),
}

impl IgnoreSource {
    fn kind(self) -> &'static str {
        match self {
            IgnoreSource::User(_) => "user",
            IgnoreSource::Team(_) => "team",
            IgnoreSource::People(_) => "people",
        }
    }

    fn id(self) -> u64 {
        match self {
            IgnoreSource::User(id) | IgnoreSource::Team(id) | IgnoreSource::People(id) => id,
        }
    }
}

/// Состояние игнора по отношению к одному пользователю.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoreFlags {
    /// Я его заблокировал.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_blocked: bool,
    /// Он заблокировал меня.
    #[serde(default, deserialize_with = "bool_lenient")]
    pub blocked_by_user: bool,
}

/// Ответ `GET /ignore/{id}`. Если пользователь в игнор-листе, есть `id` записи.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IgnoreStatus {
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub id: Option<u64>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub status: IgnoreFlags,
}

/// Запись игнор-листа.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreEntry {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Сообщение личного диалога.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessage {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub user: Option<UserRef>,
    /// Простой текст. Перед показом как HTML его надо экранировать.
    #[serde(default, deserialize_with = "null_default")]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub thread_id: u64,
    /// Номер сообщения в диалоге с единицы.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub index: u64,
}

impl ThreadMessage {
    /// Сообщение правили: время создания и правки различаются.
    pub fn is_edited(&self) -> bool {
        matches!((&self.created_at, &self.updated_at), (Some(c), Some(u)) if c != u)
    }
}

/// Сколько у стороны диалога непрочитанного и когда она читала.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub unread_count: u64,
    #[serde(default)]
    pub last_read: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadInfo {
    /// Собеседник: его `unread_count` – сколько моих сообщений он не прочитал.
    #[serde(default, deserialize_with = "null_default")]
    pub participant: ReadState,
    /// Я.
    #[serde(default, deserialize_with = "null_default")]
    pub auth: ReadState,
}

/// Диалог.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageThread {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Обычно ник собеседника, но бывает пустой.
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub participant: Option<UserRef>,
    /// У пустого диалога `None`.
    #[serde(default)]
    pub last_message: Option<ThreadMessage>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, deserialize_with = "bool_lenient")]
    pub is_archived: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub read_info: ReadInfo,
}

impl MessageThread {
    /// Заголовок диалога, как его показывает сайт: тема, иначе ник собеседника.
    pub fn title(&self) -> &str {
        self.subject
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.participant.as_ref().map(|p| p.username.as_str()))
            .unwrap_or_default()
    }

    /// Сколько сообщений я не прочитал.
    pub fn unread(&self) -> u64 {
        self.read_info.auth.unread_count
    }
}

impl Client {
    /// Список друзей или заявок пользователя, по 42 записи. `text` ищет по имени, от
    /// двух символов. Заявки сервер отдаёт только владельцу.
    pub async fn friends(&self, user_id: u64, list: FriendList, text: &str, page: u32) -> Result<Page<Friendship>> {
        let mut query: Vec<(&str, String)> = vec![
            ("page", page.max(1).to_string()),
            ("user_id", user_id.to_string()),
            ("status", if list == FriendList::Friends { "1" } else { "0" }.into()),
        ];
        match list {
            FriendList::Friends => {}
            FriendList::Incoming => query.push(("sender", "0".into())),
            FriendList::Outgoing => query.push(("sender", "1".into())),
        }
        let text = text.trim();
        if text.chars().count() >= 2 {
            query.push(("q", text.to_string()));
        }
        self.get_page(Site::MangaLib, "friendship", &query).await
    }

    /// Общие друзья с пользователем. Если их нет, `meta` и `links` в ответе тоже нет.
    pub async fn mutual_friends(&self, user_id: u64, page: u32) -> Result<Page<Friendship>> {
        self.get_page(
            Site::MangaLib,
            &format!("friendship/{user_id}/mutual"),
            &[("page", page.max(1).to_string())],
        )
        .await
    }

    /// Состояние дружбы с одним пользователем. Если дружбы нет, запись пустая, но флаги
    /// на месте.
    pub async fn friendship_with(&self, user_id: u64) -> Result<Friendship> {
        self.get_data(Site::MangaLib, &format!("friendship/{user_id}"), &())
            .await
    }

    /// Принимает или отклоняет сразу все входящие заявки.
    pub async fn friendship_bulk(&self, action: FriendBulk) -> Result<()> {
        let body = json!({ "status": action.status() });
        self.send_ok(Site::MangaLib, Method::PUT, "friendship/bulk", Some(&body))
            .await
    }

    /// Отправляет заявку в друзья. Сервер отвечает новой записью дружбы, сайт ставит
    /// её вместо прежней. `None` – запрос прошёл, но записи в ответе нет: тогда
    /// состояние перечитывают `friendship_with`.
    pub async fn friend_request(&self, user_id: u64) -> Result<Option<Friendship>> {
        let body = json!({ "recipient_id": user_id });
        self.send_record(Site::MangaLib, Method::POST, "friendship", Some(&body))
            .await
    }

    /// Принимает входящую заявку. `entry_id` – id записи дружбы (`Friendship::id`).
    pub async fn accept_friend(&self, entry_id: u64) -> Result<Option<Friendship>> {
        let body = json!({ "status": FRIEND_ACCEPT });
        self.send_record(
            Site::MangaLib,
            Method::PUT,
            &format!("friendship/{entry_id}"),
            Some(&body),
        )
        .await
    }

    /// Отклоняет входящую заявку или отзывает свою. `entry_id` – id записи дружбы.
    pub async fn decline_friend(&self, entry_id: u64) -> Result<Option<Friendship>> {
        let body = json!({ "status": FRIEND_DECLINE });
        self.send_record(
            Site::MangaLib,
            Method::PUT,
            &format!("friendship/{entry_id}"),
            Some(&body),
        )
        .await
    }

    /// Удаляет из друзей. `entry_id` – id записи дружбы, а не пользователя.
    pub async fn remove_friend(&self, entry_id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("friendship/{entry_id}"), None)
            .await
    }

    /// Отношение игнора с одним пользователем.
    pub async fn ignore_status(&self, user_id: u64) -> Result<IgnoreStatus> {
        self.get_data(Site::MangaLib, &format!("ignore/{user_id}"), &()).await
    }

    /// Игнор-лист. Без источника сервер отвечает 422, поэтому он обязателен. `text`
    /// ищет по нику или id пользователя.
    pub async fn ignore_list(&self, source: IgnoreSource, text: &str, page: u32) -> Result<Page<IgnoreEntry>> {
        let mut query: Vec<(&str, String)> = vec![
            ("page", page.max(1).to_string()),
            ("sourceable_type", source.kind().into()),
            ("sourceable_id", source.id().to_string()),
        ];
        let text = text.trim();
        if !text.is_empty() {
            query.push(("q", text.to_string()));
        }
        self.get_page(Site::MangaLib, "ignore", &query).await
    }

    /// Добавляет пользователя в игнор-лист. Для команды и автора причина обязательна:
    /// без неё модератор убирает запись.
    pub async fn ignore_add(&self, source: IgnoreSource, user_id: u64, comment: &str) -> Result<()> {
        let comment = comment.trim();
        if comment.is_empty() && !matches!(source, IgnoreSource::User(_)) {
            return Err(Error::Other(
                "для игнора от имени команды или автора нужен комментарий с причиной".into(),
            ));
        }
        let mut body = json!({
            "sourceable_id": source.id(),
            "sourceable_type": source.kind(),
            "user_id": user_id,
        });
        if !comment.is_empty() {
            body["comment"] = comment.into();
        }
        self.send_ok(Site::MangaLib, Method::POST, "ignore", Some(&body)).await
    }

    /// Меняет комментарий записи игнор-листа.
    pub async fn ignore_update(&self, entry_id: u64, comment: &str) -> Result<()> {
        let body = json!({ "comment": comment });
        self.send_ok(Site::MangaLib, Method::PUT, &format!("ignore/{entry_id}"), Some(&body))
            .await
    }

    /// Убирает запись из игнор-листа. Сервер отвечает удалённой записью, клиенту она не нужна.
    pub async fn ignore_remove(&self, entry_id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("ignore/{entry_id}"), None)
            .await
    }

    /// Диалоги, по 15 на страницу. `archived` показывает архив, `text` ищет по диалогам.
    pub async fn message_threads(&self, archived: bool, text: &str, page: u32) -> Result<Page<MessageThread>> {
        let mut query: Vec<(&str, String)> = vec![
            ("page", page.max(1).to_string()),
            ("archived", if archived { "1" } else { "0" }.into()),
        ];
        let text = text.trim();
        if !text.is_empty() {
            query.push(("q", text.to_string()));
        }
        self.get_page(Site::MangaLib, "messenger/threads", &query).await
    }

    pub async fn message_thread(&self, thread_id: u64) -> Result<MessageThread> {
        self.get_data(Site::MangaLib, &format!("messenger/threads/{thread_id}"), &())
            .await
    }

    /// Диалог с пользователем. `None`, если его нет: сайт тогда открывает пустой диалог
    /// и создаёт запись при первой отправке. Поиск находит и архивные диалоги, так что
    /// новый диалог рядом с архивным дублирует его.
    pub async fn find_message_thread(&self, user_id: u64) -> Result<Option<MessageThread>> {
        match self
            .get_data(Site::MangaLib, &format!("messenger/threads/find/{user_id}"), &())
            .await
        {
            Ok(thread) => Ok(Some(thread)),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Сообщения диалога, новые сверху. Отдельного запроса «прочитано» нет: счётчик
    /// обнуляется сам, когда сообщения запрошены. У короткого диалога `has_next` ложно
    /// из-за отсутствия `links`.
    pub async fn thread_messages(&self, thread_id: u64, page: u32) -> Result<Page<ThreadMessage>> {
        self.get_page(
            Site::MangaLib,
            &format!("messenger/threads/{thread_id}/messages"),
            &[("page", page.max(1).to_string())],
        )
        .await
    }

    /// Создаёт диалог с пользователем. Сайт делает это прямо перед первой отправкой.
    pub async fn create_message_thread(&self, user_id: u64) -> Result<MessageThread> {
        let body = json!({ "participants": [user_id] });
        self.send_data(Site::MangaLib, Method::POST, "messenger/threads", &(), Some(&body))
            .await
    }

    /// Отправляет сообщение, до 2000 символов. В архивный диалог писать нельзя.
    pub async fn send_thread_message(&self, thread_id: u64, text: &str) -> Result<ThreadMessage> {
        let body = json!({ "body": checked_message(text)? });
        self.send_data(
            Site::MangaLib,
            Method::POST,
            &format!("messenger/threads/{thread_id}/messages"),
            &(),
            Some(&body),
        )
        .await
    }

    /// Правит своё сообщение.
    pub async fn edit_thread_message(&self, thread_id: u64, message_id: u64, text: &str) -> Result<ThreadMessage> {
        let body = json!({ "body": checked_message(text)? });
        self.send_data(
            Site::MangaLib,
            Method::PUT,
            &format!("messenger/threads/{thread_id}/messages/{message_id}"),
            &(),
            Some(&body),
        )
        .await
    }

    /// Удаляет свои сообщения.
    pub async fn delete_thread_messages(&self, thread_id: u64, message_ids: &[u64]) -> Result<()> {
        if message_ids.is_empty() {
            return Ok(());
        }
        let body = json!({ "messages": message_ids });
        self.send_ok(
            Site::MangaLib,
            Method::DELETE,
            &format!("messenger/threads/{thread_id}/messages"),
            Some(&body),
        )
        .await
    }

    /// Кто из пользователей сейчас онлайн. Возвращает id и признак; тех, кого сервер
    /// не назвал, в словаре нет.
    pub async fn check_online(&self, user_ids: &[u64]) -> Result<HashMap<u64, bool>> {
        if user_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let body = json!({ "users": user_ids });
        let response = self
            .send_api(Auth::User, |http, base| {
                http.post(join_url(base, "chat/check-online"))
                    .headers(Self::site_headers(Site::MangaLib))
                    .json(&body)
            })
            .await?;
        let bytes = response.bytes().await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: "chat/check-online".into(),
            source,
        })?;
        Ok(parse_online(&value))
    }
}

/// Сообщение без пробелов по краям; пустое и слишком длинное отвергается до запроса.
fn checked_message(text: &str) -> Result<&str> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Other("сообщение пустое".into()));
    }
    if text.chars().count() > MESSAGE_MAX_CHARS {
        return Err(Error::Other(format!("сообщение длиннее {MESSAGE_MAX_CHARS} символов")));
    }
    Ok(text)
}

/// Ответ приходит словарём `{"1001": "online"}`, завёрнутым в `data` или без обёртки.
fn parse_online(value: &Value) -> HashMap<u64, bool> {
    let map = value.get("data").unwrap_or(value);
    map.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()? == "online")))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_follow_flags() {
        let s = |r, a, f| FriendshipStatus {
            is_requested: r,
            is_awaiting_confirmation: a,
            is_friend: f,
            ..Default::default()
        };
        assert_eq!(s(false, false, false).available_action(), FriendshipAction::Request);
        assert_eq!(s(false, true, false).available_action(), FriendshipAction::Accept);
        assert_eq!(s(true, false, false).available_action(), FriendshipAction::Revoke);
        assert_eq!(s(false, false, true).available_action(), FriendshipAction::Remove);
    }

    #[test]
    fn message_limits() {
        assert!(checked_message("   ").is_err());
        assert!(checked_message(&"я".repeat(MESSAGE_MAX_CHARS)).is_ok());
        assert!(checked_message(&"я".repeat(MESSAGE_MAX_CHARS + 1)).is_err());
        assert_eq!(checked_message("  привет \n").unwrap(), "привет");
    }

    #[test]
    fn online_with_and_without_wrapper() {
        let wrapped = parse_online(&json!({"data": {"1": "online", "2": "offline", "x": "online"}}));
        assert_eq!(wrapped.get(&1), Some(&true));
        assert_eq!(wrapped.get(&2), Some(&false));
        assert_eq!(wrapped.len(), 2);
        assert_eq!(parse_online(&json!({"7": "online"})).get(&7), Some(&true));
    }

    #[test]
    fn thread_title_falls_back_to_participant() {
        let t: MessageThread = serde_json::from_value(json!({
            "id": 9001, "subject": null, "participant": {"id": 5, "username": "user1"},
            "last_message": null, "is_archived": 0
        }))
        .unwrap();
        assert_eq!(t.title(), "user1");
        assert_eq!(t.unread(), 0);
        assert!(t.last_message.is_none());
    }
}
