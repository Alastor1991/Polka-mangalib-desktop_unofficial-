//! Мини-чат: история, одно сообщение, отправка и websocket по протоколу Pusher.
//! HTTP – wiremock, websocket – локальный сервер на tungstenite в отдельном потоке,
//! сообщения – синтетика по устройству настоящего ответа.

// Обработчик рукопожатия tungstenite обязан вернуть `ErrorResponse` целиком.
#![allow(clippy::result_large_err)]

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libsocial::chat::*;
use libsocial::{Client, ClientConfig, Error};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

async fn logged_in(server: &MockServer) -> Client {
    let c = client(server);
    c.set_token(Some(libsocial::Token {
        access_token: "t".into(),
        refresh_token: None,
        token_type: "Bearer".into(),
        expires_at: libsocial::auth::now_secs() + 3600,
        client_id: "1".into(),
    }))
    .await;
    c
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/chat_{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

#[tokio::test]
async fn history_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/chat"))
        .and(query_param("auth", "1"))
        .respond_with(ok(fixture("messages")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/chat"))
        .and(query_param("before", "9000002"))
        .respond_with(ok(json!({"data": {"items": [{"id": 9000003, "text": "новое"}]}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/chat"))
        .and(query_param("after", "9000001"))
        .respond_with(ok(json!({"data": []})))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;

    let latest = c.chat_messages(ChatPage::Latest).await.unwrap();
    assert_eq!(latest.iter().map(|m| m.id).collect::<Vec<_>>(), vec![9000002, 9000001]);
    let reply = &latest[0];
    assert_eq!(reply.user.user.username, "Второй");
    assert!(reply.is_online());
    assert_eq!(reply.reply_ids(), vec![9000001]);
    assert_eq!(
        reply.user.avatar_frame.as_ref().unwrap().sm.as_deref(),
        Some("/uploads/frames/1/sm.png")
    );
    assert_eq!(
        reply.doc().plain_text().lines().count(),
        2,
        "перенос <br> – вторая строка"
    );
    let first = &latest[1];
    assert!(!first.is_online());
    assert_eq!(first.user.user.avatar_url(), None, "заглушка вместо аватара");
    assert_eq!(first.reply_mention(), "@Читатель#9000001");

    let newer = c.chat_messages(ChatPage::Newer(9000002)).await.unwrap();
    assert_eq!(newer[0].id, 9000003);
    assert!(c.chat_messages(ChatPage::Older(9000001)).await.unwrap().is_empty());
}

#[tokio::test]
async fn guest_is_refused_before_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(json!([])))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok(json!([])))
        .expect(0)
        .mount(&server)
        .await;
    let c = client(&server);
    assert!(matches!(
        c.chat_messages(ChatPage::Latest).await,
        Err(Error::Unauthorized)
    ));
    assert!(matches!(c.chat_message(1).await, Err(Error::Unauthorized)));
    assert!(matches!(
        c.send_chat_message("привет", None).await,
        Err(Error::Unauthorized)
    ));
}

#[tokio::test]
async fn one_message_bare_and_wrapped() {
    let server = MockServer::start().await;
    let message = fixture("messages")[1].clone();
    Mock::given(method("GET"))
        .and(path("/api/chat/message"))
        .and(query_param("id", "9000001"))
        .respond_with(ok(message.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/chat/message"))
        .and(query_param("id", "9000001"))
        .respond_with(ok(json!({"data": message})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/chat/message"))
        .and(query_param("id", "2"))
        .respond_with(ok(
            json!({"data": {"toast": {"type": "error", "message": "Сообщение удалено"}}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/chat/message"))
        .and(query_param("id", "3"))
        .respond_with(ok(json!({})))
        .mount(&server)
        .await;
    let c = logged_in(&server).await;
    assert_eq!(c.chat_message(9000001).await.unwrap().text, "Когда выйдет глава?");
    assert_eq!(c.chat_message(9000001).await.unwrap().user.user.id, 101);
    assert!(matches!(c.chat_message(2).await, Err(Error::Other(m)) if m == "Сообщение удалено"));
    assert!(matches!(c.chat_message(3).await, Err(Error::NotFound)));
}

#[tokio::test]
async fn send_message_bodies() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_json(json!({"text": "привет", "before": 9000002})))
        .respond_with(ok(json!({"data": [{"id": 9000003, "text": "привет"}]})))
        .expect(1)
        .mount(&server)
        .await;
    // Без истории `before` не отправляется вовсе, как у сайта.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_json(json!({"text": "первое"})))
        .respond_with(ok(json!([{"id": 1, "text": "первое"}])))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;
    let sent = c.send_chat_message("  привет \n", Some(9000002)).await.unwrap();
    assert_eq!(sent.iter().map(|m| m.id).collect::<Vec<_>>(), vec![9000003]);
    assert_eq!(c.send_chat_message("первое", None).await.unwrap()[0].id, 1);
}

#[tokio::test]
async fn send_message_checks_and_answers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_json(json!({"text": "пустой ответ"})))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_json(json!({"text": "отказ"})))
        .respond_with(ok(
            json!({"data": {"toast": {"type": "error", "message": "Подождите немного"}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let c = logged_in(&server).await;

    // Сообщение ушло, а ответ без сообщений: не ошибка, иначе вызывающий повторит.
    assert!(c.send_chat_message("пустой ответ", None).await.unwrap().is_empty());
    match c.send_chat_message("отказ", None).await {
        Err(Error::Other(m)) => assert_eq!(m, "Подождите немного"),
        other => panic!("ожидался отказ: {other:?}"),
    }
    // До запроса: пустое и слишком длинное.
    assert!(matches!(c.send_chat_message("  \n", None).await, Err(Error::Other(_))));
    let long = "я".repeat(CHAT_MESSAGE_MAX_CHARS + 1);
    assert!(matches!(c.send_chat_message(&long, None).await, Err(Error::Other(_))));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// ---------- websocket ----------

type Ws = tungstenite::WebSocket<TcpStream>;
/// `Origin`, `Referer` и `Authorization` каждого подключения.
type Headers = Arc<Mutex<Vec<(String, String, String)>>>;

/// Локальный websocket-сервер. `script` получает номер подключения и сокет, заголовки
/// подключений копятся в `Headers`.
fn ws_server<F>(script: F) -> (String, Headers)
where
    F: Fn(usize, &mut Ws) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/api/app/prod_80?protocol=7", listener.local_addr().unwrap());
    let headers: Headers = Arc::default();
    let seen = headers.clone();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { return };
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let seen = seen.clone();
            let record = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                let get = |name| {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                };
                seen.lock()
                    .unwrap()
                    .push((get("origin"), get("referer"), get("authorization")));
                Ok(resp)
            };
            let Ok(mut ws) = tungstenite::accept_hdr(stream, record) else {
                return;
            };
            script(n, &mut ws);
        }
    });
    (url, headers)
}

fn frame(event: &str, channel: Option<&str>, data: Value) -> tungstenite::Message {
    let mut f = json!({"event": event, "data": data.to_string()});
    if let Some(channel) = channel {
        f["channel"] = json!(channel);
    }
    tungstenite::Message::text(f.to_string())
}

fn established(ws: &mut Ws, activity_timeout: u64) {
    let data = json!({"socket_id": "1.2", "activity_timeout": activity_timeout});
    ws.send(frame("pusher:connection_established", None, data)).unwrap();
}

/// Следующий текстовый кадр клиента; `None`, когда клиент ушёл.
fn recv(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
            Ok(tungstenite::Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

/// Принимает подписку на канал сообщений и подтверждает её.
fn accept_chat(ws: &mut Ws) {
    let sub = recv(ws).unwrap();
    assert_eq!(
        sub,
        json!({"event": "pusher:subscribe", "data": {"auth": "", "channel": "chat"}})
    );
    ws.send(frame("pusher_internal:subscription_succeeded", Some("chat"), json!({})))
        .unwrap();
}

/// Ждёт, пока клиент закроет соединение.
fn drain(ws: &mut Ws) {
    while recv(ws).is_some() {}
}

async fn next(rx: &mut mpsc::Receiver<ChatEvent>) -> ChatEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("событие не пришло")
        .expect("канал закрыт")
}

#[tokio::test]
async fn socket_messages_and_presence() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/broadcasting/auth"))
        .and(body_json(json!({"socket_id": "1.2", "channel_name": "presence-chat"})))
        .respond_with(ok(json!({"auth": "key:sig", "channel_data": "{\"user_id\":5}"})))
        .expect(1)
        .mount(&server)
        .await;
    let (url, headers) = ws_server(|_, ws| {
        established(ws, 30);
        accept_chat(ws);
        let sub = recv(ws).unwrap();
        assert_eq!(
            sub,
            json!({"event": "pusher:subscribe", "data": {
                "auth": "key:sig", "channel_data": "{\"user_id\":5}", "channel": "presence-chat"
            }})
        );
        let presence = json!({"presence": {"ids": ["5", "6"], "hash": {"5": {"id": 5}, "6": {"id": 6}}, "count": 2}});
        ws.send(frame(
            "pusher_internal:subscription_succeeded",
            Some("presence-chat"),
            presence,
        ))
        .unwrap();
        // Сервер сам проверяет, жив ли клиент.
        ws.send(frame("pusher:ping", None, json!({}))).unwrap();
        assert_eq!(recv(ws).unwrap()["event"], "pusher:pong");
        let message =
            json!({"payload": {"id": 10, "text": "привет", "online": "online", "user": {"id": 6, "username": "ник"}}});
        ws.send(frame("ChatMessageCreated", Some("chat"), message)).unwrap();
        ws.send(frame("ChatMessageDeleted", Some("chat"), json!({"id": 10})))
            .unwrap();
        ws.send(frame(
            "pusher_internal:member_added",
            Some("presence-chat"),
            json!({"user_id": 7, "user_info": {"id": 7}}),
        ))
        .unwrap();
        ws.send(frame(
            "pusher_internal:member_removed",
            Some("presence-chat"),
            json!({"user_id": "6"}),
        ))
        .unwrap();
        drain(ws);
    });
    let c = logged_in(&server).await;
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(c.chat_socket().with_url(url).run(tx));

    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    let ChatEvent::Members(members) = next(&mut rx).await else {
        panic!("нет списка участников");
    };
    assert_eq!(members.iter().map(|m| m.id).collect::<Vec<_>>(), vec![5, 6]);
    let ChatEvent::Message(m) = next(&mut rx).await else {
        panic!("нет сообщения");
    };
    assert_eq!((m.id, m.text.as_str(), m.user.user.id), (10, "привет", 6));
    assert!(matches!(next(&mut rx).await, ChatEvent::Deleted(10)));
    assert!(matches!(
        next(&mut rx).await,
        ChatEvent::MemberJoined(ChatMember { id: 7, .. })
    ));
    assert!(matches!(next(&mut rx).await, ChatEvent::MemberLeft(6)));

    // Получатель ушёл: соединение закрывается, `run` заканчивается без ошибки.
    drop(rx);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_ok());
    let headers = headers.lock().unwrap().clone();
    // Токен в websocket не уходит: он нужен только подписи канала через API.
    assert_eq!(
        headers,
        vec![(
            "https://mangalib.me".to_string(),
            "https://mangalib.me/".to_string(),
            String::new()
        )]
    );
}

#[tokio::test]
async fn guest_listens_without_presence() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok(json!({})))
        .expect(0)
        .mount(&server)
        .await;
    let (url, _) = ws_server(|_, ws| {
        established(ws, 30);
        accept_chat(ws);
        let message = json!({"payload": {"id": 11, "text": "гостю видно", "user": {"id": 1}}});
        ws.send(frame("ChatMessageCreated", Some("chat"), message)).unwrap();
        drain(ws);
    });
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(client(&server).chat_socket().with_url(url).run(tx));

    assert!(matches!(next(&mut rx).await, ChatEvent::PresenceUnavailable(_)));
    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    assert!(matches!(next(&mut rx).await, ChatEvent::Message(m) if m.id == 11));
    drop(rx);
    assert!(run.await.unwrap().is_ok());
}

#[tokio::test]
async fn socket_pings_and_reconnects_after_silence() {
    let server = MockServer::start().await;
    let (url, headers) = ws_server(|n, ws| {
        if n == 0 {
            // Сервер просит пинговать через секунду тишины и на ping не отвечает.
            established(ws, 1);
            accept_chat(ws);
            assert_eq!(recv(ws).unwrap(), json!({"event": "pusher:ping", "data": {}}));
            drain(ws);
        } else {
            established(ws, 30);
            accept_chat(ws);
            drain(ws);
        }
    });
    let (tx, mut rx) = mpsc::channel(16);
    let socket = client(&server).chat_socket().presence(false).with_url(url);
    let run = tokio::spawn(socket.run(tx));

    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    let ChatEvent::Disconnected { reason, retry_in } = next(&mut rx).await else {
        panic!("ожидался разрыв");
    };
    assert!(reason.contains("перестал отвечать"), "{reason}");
    assert!(retry_in >= Duration::from_millis(800) && retry_in <= Duration::from_millis(1200));
    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    assert_eq!(headers.lock().unwrap().len(), 2);
    drop(rx);
    assert!(run.await.unwrap().is_ok());
}

#[tokio::test]
async fn socket_stops_on_fatal_pusher_error() {
    let server = MockServer::start().await;
    let (url, headers) = ws_server(|_, ws| {
        ws.send(frame(
            "pusher:error",
            None,
            json!({"code": 4001, "message": "Application does not exist"}),
        ))
        .unwrap();
        drain(ws);
    });
    let (tx, _rx) = mpsc::channel(16);
    let socket = client(&server).chat_socket().presence(false).with_url(url);
    let result = tokio::time::timeout(Duration::from_secs(10), socket.run(tx))
        .await
        .unwrap();
    match result {
        Err(Error::Other(m)) => assert!(m.contains("Application does not exist"), "{m}"),
        other => panic!("ожидался отказ: {other:?}"),
    }
    assert_eq!(
        headers.lock().unwrap().len(),
        1,
        "после отказа клиент не переподключается"
    );
}

#[tokio::test]
async fn refused_handshake_is_retried_later() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/api/app/prod_80", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let refuse = |_: &Request, _: Response| -> Result<Response, ErrorResponse> {
                let mut resp = ErrorResponse::new(Some("blocked".into()));
                *resp.status_mut() = StatusCode::FORBIDDEN;
                Err(resp)
            };
            let _ = tungstenite::accept_hdr(stream, refuse);
        }
    });
    let server = MockServer::start().await;
    let (tx, mut rx) = mpsc::channel(16);
    let socket = client(&server).chat_socket().presence(false).with_url(url);
    let run = tokio::spawn(socket.run(tx));
    let ChatEvent::Disconnected { reason, .. } = next(&mut rx).await else {
        panic!("ожидался разрыв");
    };
    assert!(reason.contains("403"), "{reason}");
    drop(rx);
    assert!(run.await.unwrap().is_ok());
}

#[tokio::test]
async fn presence_refused_messages_still_flow() {
    let server = MockServer::start().await;
    // Явный отказ не повторяется: второй раз сервер скажет то же.
    Mock::given(method("POST"))
        .and(path("/api/broadcasting/auth"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "Forbidden"})))
        .expect(1)
        .mount(&server)
        .await;
    let (url, _) = ws_server(|_, ws| {
        established(ws, 30);
        accept_chat(ws);
        let message = json!({"payload": {"id": 12, "text": "всё равно видно", "user": {"id": 1}}});
        ws.send(frame("ChatMessageCreated", Some("chat"), message)).unwrap();
        drain(ws);
    });
    let c = logged_in(&server).await;
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(c.chat_socket().with_url(url).run(tx));

    let ChatEvent::PresenceUnavailable(reason) = next(&mut rx).await else {
        panic!("ожидался отказ в присутствии");
    };
    assert_eq!(reason, "сервер не пустил в список участников чата");
    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    assert!(matches!(next(&mut rx).await, ChatEvent::Message(m) if m.id == 12));
    drop(rx);
    assert!(run.await.unwrap().is_ok());
}

#[tokio::test]
async fn wrapped_signature_and_errors_inside_session() {
    let server = MockServer::start().await;
    // Подпись в обёртке `data`, `channel_data` объектом: в подписку она уходит строкой.
    Mock::given(method("POST"))
        .and(path("/api/broadcasting/auth"))
        .respond_with(ok(json!({"data": {"auth": "key:sig", "channel_data": {"user_id": 5}}})))
        .expect(2)
        .mount(&server)
        .await;
    let (url, _) = ws_server(|n, ws| {
        established(ws, 30);
        accept_chat(ws);
        let sub = recv(ws).unwrap();
        assert_eq!(sub["data"]["channel_data"], "{\"user_id\":5}");
        assert_eq!(sub["data"]["auth"], "key:sig");
        if n == 0 {
            let refused = json!({"type": "AuthError", "error": "Unauthorized", "status": 4302});
            ws.send(frame("pusher:subscription_error", Some("presence-chat"), refused))
                .unwrap();
            // Сервер перегружен: переподключиться позже.
            ws.send(frame(
                "pusher:error",
                None,
                json!({"code": 4100, "message": "Over capacity"}),
            ))
            .unwrap();
        } else {
            // Приложение отключено: переподключаться бесполезно.
            let frame = tungstenite::protocol::CloseFrame {
                code: 4001.into(),
                reason: "".into(),
            };
            ws.close(Some(frame)).unwrap();
        }
        drain(ws);
    });
    let c = logged_in(&server).await;
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(c.chat_socket().with_url(url).run(tx));

    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    assert!(matches!(next(&mut rx).await, ChatEvent::PresenceUnavailable(_)));
    let ChatEvent::Disconnected { reason, .. } = next(&mut rx).await else {
        panic!("ожидался разрыв");
    };
    assert!(reason.contains("Over capacity"), "{reason}");
    assert!(matches!(next(&mut rx).await, ChatEvent::Connected));
    let result = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::Other(m)) if m.contains("4001")));
}

#[tokio::test]
async fn closing_receiver_cancels_pending_signature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/broadcasting/auth"))
        .respond_with(ok(json!({"auth": "key:sig"})).set_delay(Duration::from_secs(15)))
        .mount(&server)
        .await;
    let (url, _) = ws_server(|_, ws| {
        established(ws, 30);
        accept_chat(ws);
        drain(ws);
    });
    let c = logged_in(&server).await;
    let (tx, rx) = mpsc::channel(16);
    let run = tokio::spawn(c.chat_socket().with_url(url).run(tx));
    // Подпись ещё идёт, а получатель уже ушёл: `run` не ждёт её конца.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(rx);
    let result = tokio::time::timeout(Duration::from_secs(3), run).await;
    assert!(result.expect("run ждал подпись").unwrap().is_ok());
}
