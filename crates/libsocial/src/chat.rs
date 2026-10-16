//! Мини-чат сети: история, одно сообщение, отправка и события в реальном времени.
//! Чат один на все сайты, поэтому запросы идут от MangaLib. История и отправка
//! требуют входа: гостю сервер отвечает 401.
//!
//! Новые сообщения сайт получает по websocket через сервер с протоколом Pusher
//! (`ChatSocket`): публичный канал `chat` несёт сообщения и удаления, канал
//! присутствия `presence-chat` (с подписью `POST /broadcasting/auth`) – кто сейчас в
//! чате. Кто в сети из авторов истории, без websocket спрашивают
//! `Client::check_online`. Удаление сообщения (`DELETE /chat/{id}`) и бан из чата
//! сайт показывает только с правом `panel.moderation`, клиент их не делает.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER};
use reqwest_websocket::{Message, Upgrade, WebSocket};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::client::Client;
use crate::comments::AvatarFrame;
use crate::error::{Error, Result};
use crate::forum::toast_error;
use crate::rich::Doc;
use crate::serde_util::{null_default, opt_string_lenient, u64_lenient};
use crate::site::Site;
use crate::users::UserRef;

/// Адрес websocket чата. Сервер есть только на `api.cdnlibs.org`: другие зеркала API
/// отвечают на него 403 и 404.
pub const CHAT_SOCKET_URL: &str =
    "wss://api.cdnlibs.org/api/app/prod_80?protocol=7&client=js&version=8.4.0&flash=false";
/// Самое длинное сообщение в символах. Поле ввода сайта пускает 300 единиц UTF-16
/// (эмодзи там за две); сколько символов принимает сервер, не проверено.
pub const CHAT_MESSAGE_MAX_CHARS: usize = 300;
/// Сколько сообщений сервер отдаёт за раз. Меньше – значит, старее ничего нет.
pub const CHAT_PAGE_SIZE: usize = 30;

const CHAT_CHANNEL: &str = "chat";
const PRESENCE_CHANNEL: &str = "presence-chat";
/// Сколько ждать ответа на подключение и первого кадра сервера.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Сколько ждать подписи канала присутствия. Пока она идёт, сокет не читается.
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);
/// Тишина, после которой клиент сам спрашивает сервер, жив ли он, если сервер не
/// назвал срок короче. Так же поступает pusher-js.
const MAX_ACTIVITY: Duration = Duration::from_secs(120);
const MAX_PONG_WAIT: Duration = Duration::from_secs(30);
/// Паузы между переподключениями: от секунды с удвоением до двух минут. Соединение,
/// которое продержалось минуту, сбрасывает паузу к началу.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(120);
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// Паузы перед повторной подписью канала присутствия после временного сбоя.
const PRESENCE_RETRY_MIN: Duration = Duration::from_secs(60);
const PRESENCE_RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// Сообщение чата.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    /// HTML: текст экранирован, переносы строк – `<br>`. Ответ на сообщение –
    /// упоминание `@ник#id` в тексте.
    #[serde(default, deserialize_with = "null_default")]
    pub text: String,
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub user: ChatUser,
    /// `online` или `offline` на момент ответа.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub online: Option<String>,
}

impl ChatMessage {
    pub fn doc(&self) -> Doc {
        Doc::from_html(&self.text)
    }

    pub fn is_online(&self) -> bool {
        self.online.as_deref() == Some("online")
    }

    /// Что сайт вставляет в поле ввода по кнопке «Ответить».
    pub fn reply_mention(&self) -> String {
        format!("@{}#{}", self.user.user.username, self.id)
    }

    /// На какие сообщения отвечает это: id из упоминаний `@ник#id`. Сайт находит их
    /// после начала текста, пробела или тега и показывает по ним сообщение
    /// (`Client::chat_message`).
    pub fn reply_ids(&self) -> Vec<u64> {
        let text = &self.text;
        let mut ids = Vec::new();
        // Как глобальная регулярка сайта: следующее упоминание ищется после конца
        // предыдущего, а ник может содержать пробелы и `@`.
        let mut from = 0;
        for (at, _) in text.match_indices('@') {
            if at < from {
                continue;
            }
            let starts_word = text[..at]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace() || c == '>');
            if !starts_word {
                continue;
            }
            let rest = &text[at + 1..];
            let Some(hash) = rest.find(['#', '<']) else {
                continue;
            };
            if hash == 0 || !rest[hash..].starts_with('#') {
                continue;
            }
            let digits: String = rest[hash + 1..].chars().take_while(char::is_ascii_digit).collect();
            if let Ok(id) = digits.parse() {
                ids.push(id);
                from = at + 1 + hash + 1 + digits.len();
            }
        }
        ids
    }
}

/// Автор сообщения: обычная карточка пользователя и рамка аватара.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUser {
    #[serde(flatten)]
    pub user: UserRef,
    #[serde(default)]
    pub avatar_frame: Option<AvatarFrame>,
}

/// Какую часть истории спросить. Сообщения приходят от новых к старым.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatPage {
    /// Последние `CHAT_PAGE_SIZE` сообщений.
    Latest,
    /// Сообщения новее этого id: то, что пришло за время без websocket.
    Newer(u64),
    /// Сообщения старее этого id, по `CHAT_PAGE_SIZE` за раз: «Загрузить ещё».
    Older(u64),
}

impl ChatPage {
    /// Параметры, как у сайта. Названия перевёрнуты относительно времени: список
    /// идёт от новых к старым, и `before` – это «выше в списке».
    fn query(self) -> [(&'static str, String); 1] {
        match self {
            ChatPage::Latest => [("auth", "1".into())],
            ChatPage::Newer(id) => [("before", id.to_string())],
            ChatPage::Older(id) => [("after", id.to_string())],
        }
    }
}

#[derive(Serialize)]
struct NewMessage<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<u64>,
}

impl Client {
    /// Часть истории чата, от новых сообщений к старым.
    pub async fn chat_messages(&self, page: ChatPage) -> Result<Vec<ChatMessage>> {
        if !self.is_logged_in().await {
            return Err(Error::Unauthorized);
        }
        let value: Value = self.get_json(Site::MangaLib, "chat", &page.query()).await?;
        Ok(messages_of(&value))
    }

    /// Одно сообщение: сайт показывает его во всплывающей подсказке над упоминанием.
    pub async fn chat_message(&self, id: u64) -> Result<ChatMessage> {
        if !self.is_logged_in().await {
            return Err(Error::Unauthorized);
        }
        let value: Value = self.get_json(Site::MangaLib, "chat/message", &[("id", id)]).await?;
        if let Some(error) = toast_error(&value) {
            return Err(error);
        }
        let record = value.get("data").filter(|d| d.is_object()).unwrap_or(&value);
        let message: ChatMessage = serde_json::from_value(record.clone()).map_err(|source| Error::Decode {
            context: "chat/message".into(),
            source,
        })?;
        if message.id == 0 {
            return Err(Error::NotFound);
        }
        Ok(message)
    }

    /// Отправляет сообщение. `newest` – id самого нового сообщения, которое уже есть у
    /// вызывающего: сервер отвечает сообщениями новее него, включая отправленное.
    /// Когда разбирается ответ, сообщение уже ушло. Поэтому ответ без сообщений даёт
    /// пустой список: ошибка заставила бы вызывающего отправить его второй раз. Своё
    /// сообщение придёт и здесь, и событием `ChatEvent::Message`; сайт отсекает
    /// повтор по id, вызывающему надо делать так же.
    pub async fn send_chat_message(&self, text: &str, newest: Option<u64>) -> Result<Vec<ChatMessage>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Other("сообщение пустое".into()));
        }
        if text.chars().count() > CHAT_MESSAGE_MAX_CHARS {
            return Err(Error::Other(format!(
                "сообщение длиннее {CHAT_MESSAGE_MAX_CHARS} символов"
            )));
        }
        if !self.is_logged_in().await {
            return Err(Error::Unauthorized);
        }
        let body = NewMessage { text, before: newest };
        let bytes = self
            .send_raw(Site::MangaLib, Method::POST, "chat", &(), Some(&body))
            .await?
            .bytes()
            .await?;
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(Vec::new());
        };
        let messages = messages_of(&value);
        if messages.is_empty()
            && let Some(error) = toast_error(&value)
        {
            return Err(error);
        }
        Ok(messages)
    }

    /// Подключение к чату в реальном времени, см. `ChatSocket`.
    pub fn chat_socket(&self) -> ChatSocket {
        ChatSocket {
            client: self.clone(),
            url: CHAT_SOCKET_URL.into(),
            presence: true,
        }
    }
}

/// Список сообщений в любом виде, который принимает сайт: голый массив (так отвечает
/// сервер), массив в `data` или в `items`. Сообщение, которое не разобралось или
/// осталось без id, выбрасывается.
fn messages_of(value: &Value) -> Vec<ChatMessage> {
    let inner = value.get("data").unwrap_or(value);
    let list = inner.get("items").unwrap_or(inner);
    list.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|m| serde_json::from_value::<ChatMessage>(m.clone()).ok())
                .filter(|m| m.id != 0)
                .collect()
        })
        .unwrap_or_default()
}

/// Что приходит из чата в реальном времени.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Подписка на сообщения прошла. После переподключения пропущенное за разрыв
    /// дочитывают `chat_messages(ChatPage::Newer(…))`.
    Connected,
    /// Связь потеряна, следующая попытка через `retry_in`.
    Disconnected {
        reason: String,
        retry_in: Duration,
    },
    Message(Box<ChatMessage>),
    /// Модератор удалил сообщение.
    Deleted(u64),
    /// Кто сейчас в чате. Приходит после входа в канал присутствия и после каждого
    /// переподключения.
    Members(Vec<ChatMember>),
    MemberJoined(ChatMember),
    MemberLeft(u64),
    /// Канал присутствия недоступен: нет входа или сервер не подписал канал.
    /// Сообщения приходят и без него. После временного сбоя клиент пробует войти
    /// снова через минуту, с удвоением паузы до десяти минут.
    PresenceUnavailable(String),
}

/// Участник канала присутствия. `id` – `user_id` протокола Pusher, по нему же
/// приходит `MemberLeft`. Сайт берёт id из `info.id`; у Laravel это тот же id
/// пользователя, но что сервер кладёт в `info`, вживую не снято.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMember {
    pub id: u64,
    pub info: Value,
}

/// Чат в реальном времени: websocket по протоколу Pusher 7 с переподключением.
///
/// `run` держит соединение, пока жив получатель событий, и сам переподключается
/// после обрыва с растущей паузой. Пока сервис просит не присылать запросы
/// (`Client::cooldown_remaining`), не подключается и websocket. Без входа или с
/// `presence(false)` клиент слушает только сообщения и не показывается другим в
/// списке тех, кто в чате.
///
/// Получатель обязан вычитывать события. Пока канал полон, сокет не читается, на
/// ping сервера никто не отвечает, и сервер рвёт соединение.
#[derive(Debug, Clone)]
pub struct ChatSocket {
    client: Client,
    url: String,
    presence: bool,
}

/// Чем кончилось одно соединение.
enum Ending {
    /// Получатель событий больше не слушает.
    Closed,
    /// Сервер отказал так, что переподключаться бесполезно.
    Fatal(Error),
    Lost(String),
}

impl ChatSocket {
    /// Другой адрес сервера: для тестов и на случай, если сайт его сменит.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Входить ли в канал присутствия. Сайт входит всегда, когда открыт чат.
    pub fn presence(mut self, on: bool) -> Self {
        self.presence = on;
        self
    }

    /// Держит соединение и шлёт события в `events`. Возвращает `Ok`, когда
    /// получатель закрыт, и ошибку, если сервер отказал насовсем (коды 4000–4099
    /// протокола Pusher: приложение отключено или не существует).
    pub async fn run(self, events: mpsc::Sender<ChatEvent>) -> Result<()> {
        let mut retry = RETRY_MIN;
        loop {
            if let Some(left) = self.client.cooldown_remaining() {
                tokio::select! {
                    _ = sleep(left) => {}
                    _ = events.closed() => return Ok(()),
                }
            }
            let started = Instant::now();
            let reason = match self.session(&events).await {
                Ending::Closed => return Ok(()),
                Ending::Fatal(error) => return Err(error),
                Ending::Lost(reason) => reason,
            };
            let delay;
            (delay, retry) = pause(retry, started.elapsed());
            let lost = ChatEvent::Disconnected {
                reason,
                retry_in: delay,
            };
            if events.send(lost).await.is_err() {
                return Ok(());
            }
            tokio::select! {
                _ = sleep(delay) => {}
                _ = events.closed() => return Ok(()),
            }
        }
    }

    async fn connect(&self) -> std::result::Result<WebSocket, String> {
        let http = self.client.socket_http().map_err(|e| e.to_string())?;
        // Как браузер на странице сайта: без `Referer` и `Origin` сервер отвечает 403.
        let origin = Site::MangaLib.origin();
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("{origin}/")) {
            headers.insert(REFERER, v);
        }
        if let Ok(v) = HeaderValue::from_str(&origin) {
            headers.insert(ORIGIN, v);
        }
        let response = http
            .get(&self.url)
            .headers(headers)
            .upgrade()
            .send()
            .await
            .map_err(socket_error)?;
        if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Err(format!("сервер чата ответил {}", response.status().as_u16()));
        }
        response.into_websocket().await.map_err(socket_error)
    }

    async fn session(&self, events: &mpsc::Sender<ChatEvent>) -> Ending {
        let connected = unless_closed(events, timeout(HANDSHAKE_TIMEOUT, self.connect())).await;
        let mut socket = match connected {
            None => return Ending::Closed,
            Some(Ok(Ok(socket))) => socket,
            Some(Ok(Err(reason))) => return Ending::Lost(reason),
            Some(Err(_)) => return Ending::Lost("сервер чата не ответил".into()),
        };
        let first = unless_closed(events, timeout(HANDSHAKE_TIMEOUT, socket.next())).await;
        let Some(first) = first else {
            return Ending::Closed;
        };
        let (socket_id, activity) = match first {
            Ok(Some(Ok(Message::Text(text)))) => match decode(&text) {
                Incoming::Established {
                    socket_id,
                    activity_timeout,
                } => (socket_id, activity_timeout),
                Incoming::Error { code, message } => return error_ending(code, message),
                _ => return Ending::Lost("сервер чата ответил не по протоколу".into()),
            },
            Ok(Some(Ok(Message::Close { code, reason }))) => return close_ending(code.into(), reason),
            Ok(Some(Err(e))) => return Ending::Lost(socket_error(e)),
            Ok(_) => return Ending::Lost("сервер чата ответил не по протоколу".into()),
            Err(_) => return Ending::Lost("сервер чата не ответил".into()),
        };
        let activity = activity.clamp(Duration::from_secs(1), MAX_ACTIVITY);
        let pong_wait = activity.min(MAX_PONG_WAIT);

        if let Err(reason) = send(&mut socket, subscribe(CHAT_CHANNEL, None)).await {
            return Ending::Lost(reason);
        }
        // Когда снова попробовать войти в канал присутствия после временного сбоя.
        let mut presence_retry: Option<Instant> = None;
        let mut presence_pause = PRESENCE_RETRY_MIN;
        if self.presence {
            match self.join_presence(&mut socket, &socket_id, events).await {
                Ok(true) => {
                    presence_retry = Some(Instant::now() + presence_pause);
                    presence_pause = (presence_pause * 2).min(PRESENCE_RETRY_MAX);
                }
                Ok(false) => {}
                Err(ending) => return ending,
            }
        }

        // Как pusher-js: после тишины длиной `activity` клиент шлёт ping и ждёт любого
        // кадра `pong_wait`, иначе считает соединение мёртвым.
        let mut deadline = Instant::now() + activity;
        let mut awaiting_pong = false;
        loop {
            tokio::select! {
                _ = events.closed() => {
                    let _ = socket.close(reqwest_websocket::CloseCode::Normal, None).await;
                    return Ending::Closed;
                }
                _ = sleep_until(presence_retry.unwrap_or(deadline)), if presence_retry.is_some() => {
                    presence_retry = None;
                    match self.join_presence(&mut socket, &socket_id, events).await {
                        Ok(true) => {
                            presence_retry = Some(Instant::now() + presence_pause);
                            presence_pause = (presence_pause * 2).min(PRESENCE_RETRY_MAX);
                        }
                        Ok(false) => {}
                        Err(ending) => return ending,
                    }
                }
                _ = sleep_until(deadline) => {
                    if awaiting_pong {
                        return Ending::Lost("сервер чата перестал отвечать".into());
                    }
                    if let Err(reason) = send(&mut socket, json!({"event": "pusher:ping", "data": {}})).await {
                        return Ending::Lost(reason);
                    }
                    awaiting_pong = true;
                    deadline = Instant::now() + pong_wait;
                }
                frame = socket.next() => {
                    awaiting_pong = false;
                    deadline = Instant::now() + activity;
                    let text = match frame {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close { code, reason })) => return close_ending(code.into(), reason),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return Ending::Lost(socket_error(e)),
                        None => return Ending::Lost("сервер чата закрыл соединение".into()),
                    };
                    let event = match decode(&text) {
                        Incoming::Event(event) => event,
                        Incoming::Subscribed { channel, members } => {
                            if channel == CHAT_CHANNEL {
                                ChatEvent::Connected
                            } else if let Some(members) = members {
                                ChatEvent::Members(members)
                            } else {
                                continue;
                            }
                        }
                        Incoming::SubscriptionError { channel, .. } => {
                            if channel == CHAT_CHANNEL {
                                return Ending::Lost("сервер чата не дал подписаться на сообщения".into());
                            }
                            // Подписанную подписку сервер отверг: повтор с той же подписью
                            // ничего не изменит, до переподключения обходимся без списка.
                            ChatEvent::PresenceUnavailable("сервер не пустил в список участников чата".into())
                        }
                        Incoming::Ping => {
                            if let Err(reason) = send(&mut socket, json!({"event": "pusher:pong", "data": {}})).await {
                                return Ending::Lost(reason);
                            }
                            continue;
                        }
                        Incoming::Error { code, message } => match code {
                            Some(4000..=4299) => return error_ending(code, message),
                            // Ошибка без кода касается одного кадра, соединение живо.
                            _ => continue,
                        },
                        Incoming::Established { .. } | Incoming::Other => continue,
                    };
                    if events.send(event).await.is_err() {
                        return Ending::Closed;
                    }
                }
            }
        }
    }

    /// Подписывает канал присутствия и подписывается на него. `Ok(true)` – не вышло,
    /// но стоит повторить позже; `Ok(false)` – подписка отправлена или повторять
    /// бесполезно (нет входа, сервер отказал). Неудача сообщается событием.
    async fn join_presence(
        &self,
        socket: &mut WebSocket,
        socket_id: &str,
        events: &mpsc::Sender<ChatEvent>,
    ) -> std::result::Result<bool, Ending> {
        let signed = if self.client.is_logged_in().await {
            match unless_closed(events, timeout(AUTH_TIMEOUT, self.authorize(socket_id))).await {
                None => return Err(Ending::Closed),
                Some(Ok(Ok(auth))) => Ok(auth),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(_)) => Err(Error::Other("сервер не ответил вовремя".into())),
            }
        } else {
            Err(Error::Unauthorized)
        };
        let error = match signed {
            Ok(auth) => {
                return match send(socket, subscribe(PRESENCE_CHANNEL, Some(auth))).await {
                    Ok(()) => Ok(false),
                    Err(reason) => Err(Ending::Lost(reason)),
                };
            }
            Err(error) => error,
        };
        let (reason, retry) = presence_failure(&error);
        if events
            .send(ChatEvent::PresenceUnavailable(reason.into()))
            .await
            .is_err()
        {
            return Err(Ending::Closed);
        }
        Ok(retry)
    }

    /// Подпись канала присутствия для этого соединения: `{auth, channel_data}`, как
    /// у Laravel. Сайт кладёт в подписку то, что вернул сервер, а его HTTP-клиент сам
    /// снимает обёртку `data`, поэтому годится ответ с ней и без неё.
    async fn authorize(&self, socket_id: &str) -> Result<Value> {
        let body = json!({"socket_id": socket_id, "channel_name": PRESENCE_CHANNEL});
        let bytes = self
            .client
            .send_raw(Site::MangaLib, Method::POST, "broadcasting/auth", &(), Some(&body))
            .await?
            .bytes()
            .await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: "broadcasting/auth".into(),
            source,
        })?;
        let signed = value.get("data").filter(|d| d.get("auth").is_some()).unwrap_or(&value);
        let Some(auth) = signed.get("auth").and_then(Value::as_str) else {
            return Err(Error::Other("сервер не подписал канал чата".into()));
        };
        let mut data = json!({"auth": auth});
        if let Some(channel_data) = signed.get("channel_data") {
            // Pusher ждёт строку JSON; если сервер прислал объект, упаковываем сами.
            let packed = match channel_data {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            data["channel_data"] = Value::String(packed);
        }
        Ok(data)
    }
}

fn subscribe(channel: &str, auth: Option<Value>) -> Value {
    let mut data = auth.unwrap_or_else(|| json!({"auth": ""}));
    data["channel"] = Value::String(channel.into());
    json!({"event": "pusher:subscribe", "data": data})
}

async fn send(socket: &mut WebSocket, frame: Value) -> std::result::Result<(), String> {
    socket
        .send(Message::Text(frame.to_string()))
        .await
        .map_err(socket_error)
}

/// Причина обрыва для пользователя: без текста библиотек и адреса сервера.
fn socket_error(error: reqwest_websocket::Error) -> String {
    match error {
        reqwest_websocket::Error::Handshake(reqwest_websocket::HandshakeError::UnexpectedStatusCode(status)) => {
            format!("сервер чата ответил {}", status.as_u16())
        }
        reqwest_websocket::Error::Reqwest(_) => "нет связи с сервером чата".into(),
        _ => "соединение с чатом оборвалось".into(),
    }
}

/// Ждёт `work`, но бросает его, как только получатель событий закрыт.
async fn unless_closed<T>(events: &mpsc::Sender<ChatEvent>, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        out = work => Some(out),
        _ = events.closed() => None,
    }
}

/// Пауза перед следующей попыткой и следующий шаг паузы. Соединение, которое
/// продержалось `STABLE_SESSION`, начинает паузы заново.
fn pause(retry: Duration, session: Duration) -> (Duration, Duration) {
    let retry = if session >= STABLE_SESSION { RETRY_MIN } else { retry };
    let delay = retry.mul_f64(rand::random_range(0.8..1.2)).min(RETRY_MAX);
    (delay, (retry * 2).min(RETRY_MAX))
}

/// Что сказать пользователю о неудаче подписи и стоит ли её повторить. Нет входа или
/// явный отказ сервера не пройдут и со второго раза.
fn presence_failure(error: &Error) -> (&'static str, bool) {
    match error {
        Error::Unauthorized | Error::RefreshRejected | Error::Auth(_) => ("нужен вход в аккаунт", false),
        Error::Forbidden | Error::NotFound => ("сервер не пустил в список участников чата", false),
        _ => ("не удалось войти в список участников чата, попробую позже", true),
    }
}

/// Коды протокола Pusher: 4000–4099 – переподключаться бесполезно, 4100–4199 –
/// повторить позже, 4200–4299 – повторить сразу (клиент всё равно ждёт паузу).
fn error_ending(code: Option<u16>, message: String) -> Ending {
    match code {
        Some(4000..=4099) => Ending::Fatal(Error::Other(format!("сервер чата отказал: {message}"))),
        _ => Ending::Lost(format!("сервер чата закрыл соединение ({message})")),
    }
}

fn close_ending(code: u16, reason: String) -> Ending {
    let reason = if reason.is_empty() {
        format!("код {code}")
    } else {
        reason
    };
    error_ending(Some(code), reason)
}

/// Кадр сервера, разобранный до того, что нужно соединению.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
enum Incoming {
    Established {
        socket_id: String,
        activity_timeout: Duration,
    },
    Subscribed {
        channel: String,
        members: Option<Vec<ChatMember>>,
    },
    SubscriptionError {
        channel: String,
    },
    Event(ChatEvent),
    Ping,
    Error {
        code: Option<u16>,
        message: String,
    },
    Other,
}

#[cfg(test)]
impl PartialEq for ChatEvent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ChatEvent::Message(a), ChatEvent::Message(b)) => a.id == b.id && a.text == b.text,
            (ChatEvent::Deleted(a), ChatEvent::Deleted(b)) | (ChatEvent::MemberLeft(a), ChatEvent::MemberLeft(b)) => {
                a == b
            }
            (ChatEvent::Members(a), ChatEvent::Members(b)) => a == b,
            (ChatEvent::MemberJoined(a), ChatEvent::MemberJoined(b)) => a == b,
            (ChatEvent::PresenceUnavailable(a), ChatEvent::PresenceUnavailable(b)) => a == b,
            (ChatEvent::Connected, ChatEvent::Connected) => true,
            (
                ChatEvent::Disconnected { reason: a, retry_in: x },
                ChatEvent::Disconnected { reason: b, retry_in: y },
            ) => a == b && x == y,
            _ => false,
        }
    }
}

/// Разбирает кадр сервера. Поле `data` Pusher присылает строкой JSON, но встречается
/// и объектом; годятся оба.
fn decode(text: &str) -> Incoming {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Incoming::Other;
    };
    let event = frame.get("event").and_then(Value::as_str).unwrap_or_default();
    let channel = frame
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let data = match frame.get("data") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let id_of = |v: &Value| match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    };
    match event {
        "pusher:connection_established" => Incoming::Established {
            socket_id: data.get("socket_id").and_then(Value::as_str).unwrap_or_default().into(),
            activity_timeout: Duration::from_secs(data.get("activity_timeout").and_then(Value::as_u64).unwrap_or(120)),
        },
        "pusher_internal:subscription_succeeded" => {
            let members = data.get("presence").map(|presence| {
                let hash = presence.get("hash").and_then(Value::as_object);
                let ids = presence.get("ids").and_then(Value::as_array);
                match (ids, hash) {
                    (Some(ids), _) => ids
                        .iter()
                        .filter_map(|id| {
                            let info = hash.and_then(|h| h.get(&id_key(id))).cloned().unwrap_or(Value::Null);
                            Some(ChatMember { id: id_of(id)?, info })
                        })
                        .collect(),
                    (None, Some(hash)) => hash
                        .iter()
                        .filter_map(|(id, info)| {
                            Some(ChatMember {
                                id: id.parse().ok()?,
                                info: info.clone(),
                            })
                        })
                        .collect(),
                    (None, None) => Vec::new(),
                }
            });
            Incoming::Subscribed { channel, members }
        }
        "pusher:subscription_error" => Incoming::SubscriptionError { channel },
        "pusher_internal:member_added" => match data.get("user_id").and_then(id_of) {
            Some(id) => Incoming::Event(ChatEvent::MemberJoined(ChatMember {
                id,
                info: data.get("user_info").cloned().unwrap_or(Value::Null),
            })),
            None => Incoming::Other,
        },
        "pusher_internal:member_removed" => match data.get("user_id").and_then(id_of) {
            Some(id) => Incoming::Event(ChatEvent::MemberLeft(id)),
            None => Incoming::Other,
        },
        "pusher:ping" => Incoming::Ping,
        "pusher:error" => Incoming::Error {
            code: data
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u16::try_from(c).ok()),
            message: data.get("message").and_then(Value::as_str).unwrap_or_default().into(),
        },
        // Сайт подписан на эти события с точкой в начале (`.ChatMessageCreated`):
        // так Laravel Echo слушает имя события без пространства имён.
        "ChatMessageCreated" => data
            .get("payload")
            .and_then(|p| serde_json::from_value::<ChatMessage>(p.clone()).ok())
            .filter(|m| m.id != 0)
            .map_or(Incoming::Other, |m| Incoming::Event(ChatEvent::Message(Box::new(m)))),
        "ChatMessageDeleted" => data
            .get("id")
            .and_then(id_of)
            .filter(|&id| id != 0)
            .map_or(Incoming::Other, |id| Incoming::Event(ChatEvent::Deleted(id))),
        _ => Incoming::Other,
    }
}

/// Ключ словаря `hash` для id из списка `ids`: id бывают и числами, и строками.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, text: &str) -> ChatMessage {
        serde_json::from_value(json!({"id": id, "text": text, "user": {"id": 1, "username": "ник"}})).unwrap()
    }

    #[test]
    fn reply_ids_like_site() {
        assert_eq!(message(1, "@ник#42 привет").reply_ids(), vec![42]);
        assert_eq!(message(1, "да<br>@второй ник#7 и @третий#8").reply_ids(), vec![7, 8]);
        // Внутри слова и без цифр после `#` это не упоминание.
        assert!(message(1, "почта a@b#5").reply_ids().is_empty());
        assert!(message(1, "@ник# нет").reply_ids().is_empty());
        assert!(message(1, "@<b>#5").reply_ids().is_empty());
        // Ник с пробелом и `@` – одно упоминание, как у регулярки сайта.
        assert_eq!(message(1, "@a b @c#7").reply_ids(), vec![7]);
        // Сущность с `#` в нике сайт разбирает так же криво: `@O&#039;Neil#42` даёт 39.
        assert_eq!(message(1, "@O&#039;Neil#42").reply_ids(), vec![39]);
        assert_eq!(message(9, "").reply_mention(), "@ник#9");
    }

    #[test]
    fn page_queries() {
        assert_eq!(ChatPage::Latest.query(), [("auth", "1".to_string())]);
        assert_eq!(ChatPage::Newer(5).query(), [("before", "5".to_string())]);
        assert_eq!(ChatPage::Older(5).query(), [("after", "5".to_string())]);
    }

    #[test]
    fn messages_in_any_wrapper() {
        let item = json!({"id": 3, "text": "т"});
        assert_eq!(messages_of(&json!([item, {"id": 0}, "мусор"])).len(), 1);
        assert_eq!(messages_of(&json!({"data": [item]})).len(), 1);
        assert_eq!(messages_of(&json!({"data": {"items": [item]}})).len(), 1);
        assert!(messages_of(&json!({"data": {"toast": {"message": "нет"}}})).is_empty());
    }

    #[test]
    fn decodes_connection_and_subscriptions() {
        assert_eq!(
            decode(
                r#"{"event":"pusher:connection_established","data":"{\"socket_id\":\"1.2\",\"activity_timeout\":30}"}"#
            ),
            Incoming::Established {
                socket_id: "1.2".into(),
                activity_timeout: Duration::from_secs(30)
            }
        );
        assert_eq!(
            decode(r#"{"event":"pusher_internal:subscription_succeeded","channel":"chat"}"#),
            Incoming::Subscribed {
                channel: "chat".into(),
                members: None
            }
        );
        let presence = json!({
            "event": "pusher_internal:subscription_succeeded",
            "channel": "presence-chat",
            "data": json!({"presence": {"ids": [1, "2"], "hash": {"1": {"id": 1}, "2": {"id": 2}}, "count": 2}}).to_string()
        });
        let Incoming::Subscribed {
            members: Some(members), ..
        } = decode(&presence.to_string())
        else {
            panic!("нет участников");
        };
        assert_eq!(members.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(members[1].info, json!({"id": 2}));
        assert_eq!(
            decode(
                r#"{"event":"pusher:subscription_error","channel":"presence-chat","data":{"type":"AuthError","error":"нет подписи","status":4302}}"#
            ),
            Incoming::SubscriptionError {
                channel: "presence-chat".into()
            }
        );
    }

    #[test]
    fn decodes_chat_events() {
        let created = json!({
            "event": "ChatMessageCreated",
            "channel": "chat",
            "data": json!({"payload": {"id": 10, "text": "привет", "online": "online",
                "user": {"id": 5, "username": "ник", "avatar_frame": {"sm": "/f.png"}}}}).to_string()
        });
        let Incoming::Event(ChatEvent::Message(m)) = decode(&created.to_string()) else {
            panic!("сообщение не разобралось");
        };
        assert_eq!((m.id, m.user.user.id, m.is_online()), (10, 5, true));
        assert_eq!(m.user.avatar_frame.unwrap().sm.as_deref(), Some("/f.png"));
        assert_eq!(
            decode(r#"{"event":"ChatMessageDeleted","channel":"chat","data":"{\"id\":\"10\"}"}"#),
            Incoming::Event(ChatEvent::Deleted(10))
        );
        assert_eq!(
            decode(
                r#"{"event":"pusher_internal:member_removed","channel":"presence-chat","data":"{\"user_id\":\"7\"}"}"#
            ),
            Incoming::Event(ChatEvent::MemberLeft(7))
        );
        assert_eq!(
            decode(r#"{"event":"pusher:error","data":{"code":4001,"message":"нет приложения"}}"#),
            Incoming::Error {
                code: Some(4001),
                message: "нет приложения".into()
            }
        );
        assert_eq!(decode(r#"{"event":"pusher:ping","data":{}}"#), Incoming::Ping);
        assert_eq!(decode(r#"{"event":"ChatMessageCreated","data":"{}"}"#), Incoming::Other);
        assert_eq!(decode("не json"), Incoming::Other);
    }

    #[test]
    fn pusher_codes() {
        assert!(matches!(error_ending(Some(4001), "x".into()), Ending::Fatal(_)));
        assert!(matches!(error_ending(Some(4100), "x".into()), Ending::Lost(_)));
        assert!(matches!(error_ending(Some(4200), "x".into()), Ending::Lost(_)));
        assert!(matches!(close_ending(1006, String::new()), Ending::Lost(r) if r.contains("код 1006")));
    }

    #[test]
    fn pauses_grow_and_reset() {
        let (delay, next) = pause(RETRY_MIN, Duration::ZERO);
        assert!(delay >= Duration::from_millis(800) && delay <= Duration::from_millis(1200));
        assert_eq!(next, Duration::from_secs(2));
        let (delay, next) = pause(RETRY_MAX, Duration::ZERO);
        assert!(delay <= RETRY_MAX, "пауза не длиннее двух минут");
        assert_eq!(next, RETRY_MAX);
        let (delay, next) = pause(RETRY_MAX, STABLE_SESSION);
        assert!(delay <= Duration::from_millis(1200));
        assert_eq!(next, Duration::from_secs(2));
    }

    #[test]
    fn presence_failures() {
        assert!(!presence_failure(&Error::Unauthorized).1);
        assert!(!presence_failure(&Error::Forbidden).1);
        assert!(presence_failure(&Error::Other("сервер не ответил вовремя".into())).1);
        assert!(
            presence_failure(&Error::ServiceDown {
                status: reqwest::StatusCode::BAD_GATEWAY
            })
            .1
        );
    }

    #[test]
    fn subscribe_frames() {
        assert_eq!(
            subscribe("chat", None),
            json!({"event": "pusher:subscribe", "data": {"auth": "", "channel": "chat"}})
        );
        assert_eq!(
            subscribe("presence-chat", Some(json!({"auth": "k:s", "channel_data": "{}"}))),
            json!({"event": "pusher:subscribe", "data": {"auth": "k:s", "channel_data": "{}", "channel": "presence-chat"}})
        );
    }
}
