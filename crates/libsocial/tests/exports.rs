//! Серверный экспорт книг: создание, опрос, скачивание. Сервер – wiremock.
//! Схема ответов взята из скриптов сайта и вживую не проверена (нужен Premium),
//! фикстуры синтетические.

use std::time::Duration;

use libsocial::exports::{BookExport, ExportFormat, ExportRequest, ExportStatus};
use libsocial::{Client, ClientConfig, Error};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

fn export(server_status: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut body = json!({"id": 77, "manga_id": 14841, "status": server_status, "format": "epub", "poll_after": 0});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    json!({"data": body})
}

#[tokio::test]
async fn settings_come_from_constants() {
    let server = MockServer::start().await;
    Mock::given(path("/api/constants"))
        .and(query_param("fields[]", "bookExportFormats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "bookExportEnabled": true, "bookExportPremiumOnly": true, "bookExportFormats": ["epub", "fb2"]}})))
        .mount(&server)
        .await;
    let settings = client(&server).book_export_settings().await.unwrap();
    assert!(settings.enabled && settings.premium_only);
    assert!(settings.supports(ExportFormat::Fb2) && !settings.supports(ExportFormat::Mobi));
}

#[tokio::test]
async fn list_requires_title_and_skips_broken_items() {
    let server = MockServer::start().await;
    Mock::given(path("/api/book-exports"))
        .and(method("GET"))
        .and(query_param("manga_id", "14841"))
        .and(header("Site-Id", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": 1, "status": "completed"}, "мусор", {"id": 2, "status": "queued", "queue_position": 3}]})))
        .expect(1)
        .mount(&server)
        .await;
    let list = client(&server).book_exports(14841).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[1].queue_position, Some(3));
}

#[tokio::test]
async fn create_sends_format_branch_and_volume() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/book-exports"))
        .and(header("Site-Id", "3"))
        .and(body_partial_json(
            json!({"manga_id": 14841, "format": "fb2", "branch_id": 9824, "volume": "2"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(export("queued", json!({"queue_position": 1}))))
        .expect(1)
        .mount(&server)
        .await;
    let request = ExportRequest {
        manga_id: 14841,
        format: ExportFormat::Fb2,
        branch_id: Some(9824),
        volume: Some("2".into()),
    };
    let created = client(&server).create_book_export(&request).await.unwrap();
    assert_eq!(created.id, "77");
    assert_eq!(created.status, ExportStatus::Queued);
}

#[tokio::test]
async fn any_branch_and_all_volumes_are_null() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/book-exports"))
        .and(body_partial_json(json!({"branch_id": null, "volume": null})))
        .respond_with(ResponseTemplate::new(200).set_body_json(export("queued", json!({}))))
        .expect(1)
        .mount(&server)
        .await;
    let request = ExportRequest {
        manga_id: 1,
        format: ExportFormat::Epub,
        branch_id: Some(0),
        volume: None,
    };
    client(&server).create_book_export(&request).await.unwrap();
}

#[tokio::test]
async fn wait_polls_until_completed() {
    let server = MockServer::start().await;
    Mock::given(path("/api/book-exports/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(export("processing", json!({"progress": 40}))))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(path("/api/book-exports/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(export("completed", json!({"progress": 100}))))
        .mount(&server)
        .await;
    let mut seen = Vec::new();
    let done = client(&server)
        .wait_book_export("77", Duration::from_secs(30), |e| seen.push(e.progress as u32))
        .await
        .unwrap();
    assert!(done.is_completed());
    assert_eq!(seen, [40, 40, 100]);
}

#[tokio::test]
async fn wait_reports_failure_reason() {
    let server = MockServer::start().await;
    Mock::given(path("/api/book-exports/77"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(export("failed", json!({"error": {"message": "нет глав"}}))),
        )
        .mount(&server)
        .await;
    let err = client(&server)
        .wait_book_export("77", Duration::from_secs(30), |_| {})
        .await
        .unwrap_err();
    assert!(err.to_string().contains("нет глав"), "{err}");
}

#[tokio::test]
async fn wait_gives_up_after_deadline() {
    let server = MockServer::start().await;
    Mock::given(path("/api/book-exports/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(export("converting", json!({"poll_after": 5}))))
        .mount(&server)
        .await;
    let err = client(&server)
        .wait_book_export("77", Duration::from_secs(1), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Other(_)), "{err}");
}

#[tokio::test]
async fn deleted_task_is_not_found_and_delete_uses_delete() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/book-exports/9"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "нет"})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/book-exports/9"))
        .and(header("Site-Id", "3"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    assert!(matches!(c.book_export("9").await, Err(Error::NotFound)));
    c.delete_book_export("9").await.unwrap();
}

#[tokio::test]
async fn segments_are_joined_in_order_and_checked_by_size() {
    let server = MockServer::start().await;
    for (i, (key, data)) in [("k0", "AAA"), ("k1", "BB")].into_iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/api/book-exports/77/segments/{i}")))
            .and(query_param("v", key))
            .and(header("Site-Id", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(data))
            .mount(&server)
            .await;
    }
    let ready: BookExport = serde_json::from_value(
        export(
            "completed",
            json!({"delivery": "segments", "file": {"name": "book.epub", "size": 5},
                   "segments": [{"key": "k0", "ready": true, "size": 3}, {"key": "k1", "ready": true, "size": 2}]}),
        )["data"]
            .clone(),
    )
    .unwrap();
    let books = client(&server).download_book_export(&ready).await.unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].name, "book.epub");
    assert_eq!(&books[0].bytes[..], b"AAABB");

    // Размер не сошёлся: файл не отдаётся.
    let wrong: BookExport = serde_json::from_value(
        export(
            "completed",
            json!({"delivery": "segments",
                   "segments": [{"key": "k0", "ready": true, "size": 3}, {"key": "k1", "ready": true, "size": 9}]}),
        )["data"]
            .clone(),
    )
    .unwrap();
    assert!(client(&server).download_book_export(&wrong).await.is_err());
}

#[tokio::test]
async fn expired_segment_is_an_http_error() {
    let server = MockServer::start().await;
    Mock::given(path("/api/book-exports/77/segments/0"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({"message": "устарел"})))
        .mount(&server)
        .await;
    let err = client(&server).book_export_segment("77", 0, "k0").await.unwrap_err();
    assert!(
        matches!(err, Error::Http { status, .. } if status.as_u16() == 410),
        "{err}"
    );
}

#[tokio::test]
async fn parts_are_downloaded_by_links() {
    let server = MockServer::start().await;
    for part in [0, 1] {
        Mock::given(method("POST"))
            .and(path("/api/book-exports/77/link"))
            .and(query_param("part", part.to_string()))
            .and(header("Site-Id", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "url": format!("{}/files/part{part}.epub", server.uri()), "file_name": format!("book-{part}.epub")})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/files/part{part}.epub")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(format!("data{part}")))
            .expect(1)
            .mount(&server)
            .await;
    }
    let ready: BookExport = serde_json::from_value(
        export(
            "completed",
            json!({"parts": [{"index": 0, "ready": true, "name": "a"}, {"index": 1, "ready": true}, {"index": 2, "ready": false}]}),
        )["data"]
            .clone(),
    )
    .unwrap();
    let books = client(&server).download_book_export(&ready).await.unwrap();
    assert_eq!(books.len(), 2);
    assert_eq!(
        (books[0].name.as_str(), &books[0].bytes[..]),
        ("book-0.epub", &b"data0"[..])
    );
    assert_eq!(&books[1].bytes[..], b"data1");
}

#[tokio::test]
async fn unfinished_export_is_not_downloaded() {
    let server = MockServer::start().await;
    let queued: BookExport = serde_json::from_value(export("queued", json!({}))["data"].clone()).unwrap();
    assert!(client(&server).download_book_export(&queued).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}
