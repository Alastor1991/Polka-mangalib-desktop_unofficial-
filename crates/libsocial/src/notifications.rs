//! Уведомления: счётчики по вкладкам, лента, отметка прочитанным и удаление.
//!
//! Нужен вход. Приходят уведомления со всех сайтов сети сразу, сайт записи лежит в
//! `data.site_id` или `data.media.site`. Главные ловушки сервиса: у списка три
//! обязательных параметра (`notification_type`, `read_type`, `sort_type`), без первых
//! двух сервер отвечает 422, а без `sort_type` падает с 500; `per_page` он игнорирует
//! и всегда отдаёт по 20. Поле `is_read` в записях не встречалось ни разу: прочитанность
//! определяется выбранным `read_type`. Типов уведомлений в скриптах больше, чем
//! разобрано здесь, поэтому у `Notification` всегда остаётся готовый `content`.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::error::Result;
use crate::listing::{Page, SortOrder, opt_bool_lenient, opt_u8_lenient};
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;
use crate::users::UserRef;

/// Вкладка уведомлений.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationTab {
    #[default]
    All,
    Chapter,
    Episode,
    /// Ответы на комментарии.
    Comments,
    /// Личные сообщения.
    Message,
    Card,
    Other,
}

impl NotificationTab {
    pub const ALL: [NotificationTab; 7] = [
        NotificationTab::All,
        NotificationTab::Chapter,
        NotificationTab::Episode,
        NotificationTab::Comments,
        NotificationTab::Message,
        NotificationTab::Card,
        NotificationTab::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NotificationTab::All => "all",
            NotificationTab::Chapter => "chapter",
            NotificationTab::Episode => "episode",
            NotificationTab::Comments => "comments",
            NotificationTab::Message => "message",
            NotificationTab::Card => "card",
            NotificationTab::Other => "other",
        }
    }

    /// Вкладка по строке сервера, например по `Notification::category`.
    pub fn parse(s: &str) -> Option<NotificationTab> {
        NotificationTab::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// Название вкладки, как на сайте.
    pub fn label(self) -> &'static str {
        match self {
            NotificationTab::All => "Все",
            NotificationTab::Chapter => "Главы",
            NotificationTab::Episode => "Эпизоды",
            NotificationTab::Comments => "Ответы",
            NotificationTab::Message => "Личка",
            NotificationTab::Card => "Карты",
            NotificationTab::Other => "Другое",
        }
    }
}

/// Какие уведомления показывать по прочитанности.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadFilter {
    #[default]
    All,
    Unread,
    Read,
}

impl ReadFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadFilter::All => "all",
            ReadFilter::Unread => "unread",
            ReadFilter::Read => "read",
        }
    }

    /// Значение `is_read` для массового удаления: `null` для всех.
    fn is_read(self) -> Value {
        match self {
            ReadFilter::All => Value::Null,
            ReadFilter::Unread => Value::Bool(false),
            ReadFilter::Read => Value::Bool(true),
        }
    }
}

/// Счётчики по вкладкам. `chapter_player` в интерфейсе сайта не показывается.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCounts {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub all: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub chapter: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub chapter_player: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub episode: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comments: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub message: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub card: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub other: u64,
}

impl TabCounts {
    pub fn get(&self, tab: NotificationTab) -> u64 {
        match tab {
            NotificationTab::All => self.all,
            NotificationTab::Chapter => self.chapter,
            NotificationTab::Episode => self.episode,
            NotificationTab::Comments => self.comments,
            NotificationTab::Message => self.message,
            NotificationTab::Card => self.card,
            NotificationTab::Other => self.other,
        }
    }

    pub fn get_mut(&mut self, tab: NotificationTab) -> &mut u64 {
        match tab {
            NotificationTab::All => &mut self.all,
            NotificationTab::Chapter => &mut self.chapter,
            NotificationTab::Episode => &mut self.episode,
            NotificationTab::Comments => &mut self.comments,
            NotificationTab::Message => &mut self.message,
            NotificationTab::Card => &mut self.card,
            NotificationTab::Other => &mut self.other,
        }
    }
}

/// Счётчики уведомлений: непрочитанные, прочитанные и все вместе.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationCounts {
    #[serde(default, deserialize_with = "null_default")]
    pub unread: TabCounts,
    #[serde(default, deserialize_with = "null_default")]
    pub read: TabCounts,
    #[serde(default, deserialize_with = "null_default")]
    pub all: TabCounts,
}

/// Запрос к ленте. По умолчанию – первая страница всех уведомлений, новые сверху.
#[derive(Debug, Clone, Default)]
pub struct NotificationsQuery {
    pub tab: NotificationTab,
    pub read: ReadFilter,
    pub order: SortOrder,
    pub page: u32,
    /// Поиск по тексту уведомления, от двух символов.
    pub text: String,
    /// Точный тип записи, например `card.dropped`. Произвольное значение сервер
    /// отвергает с 422. Сайт шлёт его вместе с `sort_by=created_at`.
    pub exact_type: Option<String>,
}

impl NotificationsQuery {
    fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![
            ("page", self.page.max(1).to_string()),
            ("notification_type", self.tab.as_str().into()),
            ("read_type", self.read.as_str().into()),
            ("sort_type", self.order.as_str().into()),
        ];
        let text = self.text.trim();
        if text.chars().count() >= 2 {
            query.push(("q", text.to_string()));
        }
        if let Some(t) = self.exact_type.as_deref().filter(|t| !t.is_empty()) {
            query.push(("type", t.to_string()));
            query.push(("sort_by", "created_at".into()));
        }
        query
    }
}

/// Тайтл в уведомлении: коротко, обложку берите из `data` целиком, если нужна.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoticeMedia {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub rus_name: Option<String>,
    #[serde(default)]
    pub eng_name: Option<String>,
    /// `manga`, `anime` и так далее.
    #[serde(default, deserialize_with = "null_default")]
    pub model: String,
    #[serde(default, deserialize_with = "null_default")]
    pub slug_url: String,
    #[serde(default, deserialize_with = "opt_u8_lenient")]
    pub site: Option<u8>,
}

impl NoticeMedia {
    /// Русское название, если есть.
    pub fn display_name(&self) -> &str {
        self.rus_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    pub fn site(&self) -> Option<Site> {
        self.site.and_then(Site::from_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoticeTeam {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub slug_url: Option<String>,
}

/// Глава из уведомления.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoticeChapter {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub branch_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub manga_id: Option<u64>,
}

/// Новые главы тайтла. При `count` больше единицы сайт открывает страницу тайтла, а не
/// конкретную главу.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChapterNotice {
    #[serde(default)]
    pub chapter: Option<NoticeChapter>,
    #[serde(default)]
    pub media: Option<NoticeMedia>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub teams: Vec<NoticeTeam>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub count: u64,
}

impl ChapterNotice {
    /// Вышло несколько глав сразу: вести надо на страницу тайтла.
    pub fn opens_title(&self) -> bool {
        self.count > 1
    }
}

/// Новая озвучка серии.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerNotice {
    #[serde(default)]
    pub media: Option<NoticeMedia>,
    #[serde(default)]
    pub episode: Option<Value>,
    #[serde(default)]
    pub player: Option<Value>,
    #[serde(default)]
    pub team: Option<NoticeTeam>,
}

/// Комментарий из уведомления: куда он ведёт.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NoticeComment {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub parent_comment: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub root_id: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub post_page: Option<u64>,
    #[serde(default, deserialize_with = "null_default")]
    pub relation_type: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub relation_id: Option<u64>,
}

/// Ответ на комментарий или комментарий в подписке.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommentNotice {
    #[serde(default)]
    pub comment: Option<NoticeComment>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub count: u64,
    #[serde(default, deserialize_with = "opt_u8_lenient")]
    pub site_id: Option<u8>,
}

/// Что случилось с дружбой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendEvent {
    /// Мне прислали заявку.
    Request,
    /// Мою заявку приняли.
    Accept,
    /// Мою заявку отклонили.
    Deny,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FriendNotice {
    #[serde(default)]
    pub friendship: Option<Value>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default, deserialize_with = "opt_u8_lenient")]
    pub site_id: Option<u8>,
}

impl FriendNotice {
    pub fn friendship_id(&self) -> Option<u64> {
        self.friendship.as_ref()?.get("id")?.as_u64()
    }
}

/// Тип уведомления, разобранный из `type` и `data`. Если разобрать не удалось, остаётся
/// `Other` с исходным названием типа: показывайте `Notification::text`.
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    /// Вышли главы: `chapter`.
    Chapter(ChapterNotice),
    /// Новая озвучка: `player.created`.
    PlayerCreated(PlayerNotice),
    /// `comment`, `comment.subscription`, `chapter_comment`, `manga_comment`, `post_comment`.
    Comment(CommentNotice),
    /// Сообщение на форуме или подписка на автора: `forum_post`, `subscription`.
    Forum {
        discussion_id: Option<u64>,
        /// `user` значит подписку на автора. Есть только у `subscription`.
        subscription_type: Option<String>,
    },
    /// Новое личное сообщение: `messenger.new_message`.
    Message { thread_id: Option<u64> },
    /// Дружба: `friend_request`, `friend_accept`, `friend_deny`.
    Friend { event: FriendEvent, notice: FriendNotice },
    /// Выпала карта или состоялся обмен: `card.dropped`, `card.trade.*`.
    Card {
        kind: String,
        media: Option<NoticeMedia>,
        trade_order_id: Option<u64>,
    },
    /// Решение модерации: `chapter_accept`, `manga_deny`, `changelog.accept` и другие.
    Moderation { kind: String, accepted: bool },
    /// Приглашение в команду: `team.user.invite_send`.
    TeamInvite,
    /// Сообщение администрации: `admin.message`, ссылка в `link`.
    Admin { link: Option<String> },
    /// Платная подписка: окончание, выдача, подарок.
    Premium { kind: String, giver: Option<UserRef> },
    /// Остальное, в том числе типы, которых нет в этом списке.
    Other { kind: String },
}

/// Запись ленты.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Точный тип: `chapter`, `comment`, `friend_request` и так далее.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub kind: String,
    /// Вкладка записи, строкой: список вкладок может расти.
    #[serde(default, deserialize_with = "null_default")]
    pub category: String,
    /// Данные записи, структура зависит от типа. Разобранный вид – `Notification::event`.
    #[serde(default)]
    pub data: Value,
    #[serde(default, deserialize_with = "opt_bool_lenient")]
    pub yaoi: Option<bool>,
    /// Готовый текст как документ редактора. Разобранный – `Notification::text`.
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Сайт обычно это поле не присылает: берите прочитанность из выбранного фильтра.
    #[serde(default, deserialize_with = "opt_bool_lenient")]
    pub is_read: Option<bool>,
}

fn parse<T: serde::de::DeserializeOwned>(v: &Value) -> Option<T> {
    serde_json::from_value(v.clone()).ok()
}

impl Notification {
    /// Текст уведомления, готовый к показу.
    pub fn text(&self) -> Doc {
        self.content.as_ref().map(Doc::from_prosemirror).unwrap_or_default()
    }

    /// Сайт, к которому относится запись: по `data.site_id`, иначе по `data.media.site`.
    pub fn site(&self) -> Option<Site> {
        let id = self
            .data
            .get("site_id")
            .or_else(|| self.data.get("media")?.get("site"))
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))?;
        Site::from_id(u8::try_from(id).ok()?)
    }

    /// Разбирает `data` по типу записи.
    pub fn event(&self) -> NotificationEvent {
        let other = || NotificationEvent::Other {
            kind: self.kind.clone(),
        };
        let kind = self.kind.as_str();
        match kind {
            "chapter" => parse(&self.data).map(NotificationEvent::Chapter).unwrap_or_else(other),
            "player.created" => parse(&self.data)
                .map(NotificationEvent::PlayerCreated)
                .unwrap_or_else(other),
            "comment" | "comment.subscription" | "chapter_comment" | "manga_comment" | "post_comment" => {
                parse(&self.data).map(NotificationEvent::Comment).unwrap_or_else(other)
            }
            "forum_post" | "subscription" => NotificationEvent::Forum {
                discussion_id: self.data.get("discussion").and_then(|d| d.get("id")?.as_u64()),
                subscription_type: self
                    .data
                    .get("subscription_type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "messenger.new_message" => NotificationEvent::Message {
                thread_id: self.data.get("thread").and_then(|t| t.get("id")?.as_u64()),
            },
            "friend_request" | "friend_accept" | "friend_deny" => match parse::<FriendNotice>(&self.data) {
                Some(notice) => NotificationEvent::Friend {
                    event: match kind {
                        "friend_request" => FriendEvent::Request,
                        "friend_accept" => FriendEvent::Accept,
                        _ => FriendEvent::Deny,
                    },
                    notice,
                },
                None => other(),
            },
            k if k == "card.dropped" || k.starts_with("card.trade.") => NotificationEvent::Card {
                kind: k.to_string(),
                media: self.data.get("media").and_then(parse::<NoticeMedia>),
                trade_order_id: self
                    .data
                    .get("trade_order_id")
                    .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())),
            },
            "chapter_accept"
            | "chapter_deny"
            | "manga_accept"
            | "manga_deny"
            | "manga_destroy"
            | "manga_changes_accept"
            | "manga_changes_deny"
            | "changelog.accept"
            | "changelog.deny" => NotificationEvent::Moderation {
                kind: kind.to_string(),
                accepted: kind.ends_with("accept"),
            },
            "team.user.invite_send" => NotificationEvent::TeamInvite,
            "admin.message" => NotificationEvent::Admin {
                link: self.data.get("link").and_then(Value::as_str).map(str::to_string),
            },
            "premium_subscription_end_in_3_days"
            | "premium_subscription_end_today"
            | "premium_subscription_manual_granted"
            | "premium_subscription_gift_received" => NotificationEvent::Premium {
                kind: kind.to_string(),
                giver: self.data.get("giver").and_then(parse::<UserRef>),
            },
            _ => other(),
        }
    }
}

impl Client {
    /// Счётчики по вкладкам. Сайт зовёт их при старте и при смене пользователя, а по
    /// таймеру не опрашивает; для опроса хватит раза в несколько минут.
    pub async fn notification_counts(&self) -> Result<NotificationCounts> {
        self.get_data(Site::MangaLib, "notifications/count", &()).await
    }

    /// Страница ленты, по 20 записей. Ответ не зависит от сайта: приходят уведомления
    /// всех сайтов сети.
    pub async fn notifications(&self, q: &NotificationsQuery) -> Result<Page<Notification>> {
        self.get_page(Site::MangaLib, "notifications", &q.to_query()).await
    }

    /// Отмечает одно уведомление прочитанным.
    pub async fn mark_notification_read(&self, id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::PUT, &format!("notifications/{id}"), None)
            .await
    }

    /// Отмечает прочитанными все уведомления вкладки. Для всех сразу – `NotificationTab::All`.
    pub async fn mark_notifications_read(&self, tab: NotificationTab) -> Result<()> {
        let body = json!({ "notification_type": tab.as_str() });
        self.send_ok(Site::MangaLib, Method::PUT, "notifications/bulk", Some(&body))
            .await
    }

    pub async fn delete_notification(&self, id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("notifications/{id}"), None)
            .await
    }

    /// Удаляет уведомления вкладки. `read` ограничивает удаление прочитанными или
    /// непрочитанными, `ReadFilter::All` удаляет всё.
    pub async fn delete_notifications(&self, tab: NotificationTab, read: ReadFilter) -> Result<()> {
        let body = json!({ "notification_type": tab.as_str(), "is_read": read.is_read() });
        self.send_ok(Site::MangaLib, Method::DELETE, "notifications/bulk", Some(&body))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(kind: &str, data: Value) -> Notification {
        serde_json::from_value(json!({"id": 1, "type": kind, "category": "other", "data": data})).unwrap()
    }

    #[test]
    fn events_are_typed() {
        let n = note(
            "chapter",
            json!({"chapter": {"id": 4, "volume": "1", "number": 216, "manga_id": 7}, "count": 3,
                   "media": {"id": 7, "name": "Title", "slug_url": "7--t", "site": 2}}),
        );
        let NotificationEvent::Chapter(c) = n.event() else {
            panic!("не глава")
        };
        assert!(c.opens_title());
        assert_eq!(c.chapter.unwrap().number, "216");
        assert_eq!(n.site(), Some(Site::SlashLib));

        let f = note(
            "friend_request",
            json!({"friendship": {"id": 9, "status": 0}, "site_id": 1}),
        );
        match f.event() {
            NotificationEvent::Friend { event, notice } => {
                assert_eq!(event, FriendEvent::Request);
                assert_eq!(notice.friendship_id(), Some(9));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            note("manga_deny", json!({})).event(),
            NotificationEvent::Moderation { accepted: false, .. }
        ));
        assert!(matches!(
            note("messenger.new_message", json!({"thread": {"id": 5}})).event(),
            NotificationEvent::Message { thread_id: Some(5) }
        ));
        assert!(matches!(
            note("что-то новое", json!(null)).event(),
            NotificationEvent::Other { .. }
        ));
    }

    #[test]
    fn query_always_has_required_params() {
        let q = NotificationsQuery::default().to_query();
        let keys: Vec<_> = q.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["page", "notification_type", "read_type", "sort_type"]);
        let q = NotificationsQuery {
            text: "я".into(),
            exact_type: Some("card.dropped".into()),
            ..Default::default()
        }
        .to_query();
        assert!(!q.iter().any(|(k, _)| *k == "q"));
        assert!(q.contains(&("sort_by", "created_at".into())));
    }
}
