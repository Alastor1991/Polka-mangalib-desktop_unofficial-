//! Ранобэ: разбор глав двух видов и запросы с `Site-Id: 3`. Сервер – wiremock.
//! Живые проверки помечены `#[ignore]`.

use std::time::Duration;

use libsocial::ranobe::{ContentFormat, ParagraphSpan, RanobeChapter};
use libsocial::rich::Block;
use libsocial::{Client, ClientConfig, Error};
use serde_json::Value;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    let config = ClientConfig {
        api_mirrors: vec![format!("{}/api", server.uri())],
        api_interval: Duration::ZERO,
        ..Default::default()
    };
    Client::new(config).unwrap()
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn html_chapter_takes_images_from_src() {
    let chapter = RanobeChapter::from_value(&fixture("ranobe_chapter_html.json")["data"]).unwrap();
    assert_eq!(chapter.format, ContentFormat::Html);
    assert_eq!(
        (chapter.id, chapter.manga_id, chapter.branch_id),
        (2089142, Some(14841), Some(9824))
    );
    assert_eq!(chapter.title(), "Том 1 Глава 0 – Пролог");
    // В attachments путь с неверным slug, в src с верным.
    assert_eq!(
        chapter.image_urls(),
        ["https://ranobelib.me/uploads/ranobe/old-slug-novel/chapters/2089142/pic.jpg"]
    );
    assert_eq!(
        chapter.attachments[0].original_address(),
        "https://ranobelib.me/uploads/ranobe/new-slug-novel/chapters/2089142/pic.jpg"
    );
    assert_eq!(chapter.paragraph_count(), 3);
    assert_eq!(chapter.doc.blocks.len(), chapter.spans.len());
    assert_eq!(chapter.spans[0], ParagraphSpan::NONE);
    assert_eq!(chapter.spans[2], ParagraphSpan { first: 2, count: 1 });
    // Перенос исходника не остаётся в тексте, сущности декодированы.
    let text = chapter.doc.plain_text();
    assert!(text.contains("«Первая строка вторая строка & конец»."), "{text}");
}

#[test]
fn prosemirror_chapter_resolves_attachments_and_numbers() {
    let chapter = RanobeChapter::from_value(&fixture("ranobe_chapter_pm.json")["data"]).unwrap();
    assert_eq!(chapter.format, ContentFormat::ProseMirror);
    assert_eq!(chapter.title(), "Том 14 Глава 531");
    assert!(chapter.is_liked && chapter.is_viewed);
    assert_eq!(chapter.doc.blocks.len(), chapter.spans.len());
    assert_eq!(
        chapter.image_urls(),
        [
            "https://ranobelib.me/uploads/ranobe/14841/chapters/4493692/8c85eaac-0000.png",
            "https://ranobelib.me/uploads/ranobe/14841/chapters/4493692/2f23906e-0000.png",
        ]
    );
    // Абзацы: «Первый», «Курсив», пустой, «Последний». Заголовок и картинки не считаются.
    assert_eq!(chapter.paragraph_count(), 4);
    let last = chapter.block_of_paragraph(4).unwrap();
    assert_eq!(
        chapter.doc.blocks[last].clone(),
        chapter.doc.blocks[chapter.doc.blocks.len() - 1]
    );
    assert!(matches!(&chapter.doc.blocks[chapter.block_of_paragraph(3).unwrap()], Block::Paragraph(i) if i.len() == 1));
}

#[tokio::test]
async fn chapter_request_sends_site_3_and_branch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/manga/14841--novel/chapter"))
        .and(header("Site-Id", "3"))
        .and(header("Referer", "https://ranobelib.me/"))
        .and(query_param("volume", "1"))
        .and(query_param("number", "0"))
        .and(query_param("branch_id", "9824"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("ranobe_chapter_html.json")))
        .expect(1)
        .mount(&server)
        .await;
    let chapter = client(&server)
        .ranobe_chapter("14841--novel", "1", "0", Some(9824))
        .await
        .unwrap();
    assert_eq!(chapter.id, 2089142);
}

#[tokio::test]
async fn chapter_without_branch_omits_parameter() {
    let server = MockServer::start().await;
    Mock::given(path("/api/manga/1--n/chapter"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("ranobe_chapter_pm.json")))
        .mount(&server)
        .await;
    client(&server).ranobe_chapter("1--n", "14", "531", None).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].url.query().unwrap_or_default().contains("branch_id"));
}

#[tokio::test]
async fn missing_chapter_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(path("/api/manga/1--n/chapter"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "нет"})))
        .mount(&server)
        .await;
    let err = client(&server)
        .ranobe_chapter("1--n", "9", "9", None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound), "{err}");
}

#[tokio::test]
async fn chapter_list_and_branches_use_site_3() {
    let server = MockServer::start().await;
    Mock::given(path("/api/manga/1--n/chapters"))
        .and(header("Site-Id", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("ranobe_chapters.json")))
        .mount(&server)
        .await;
    Mock::given(path("/api/branches/1"))
        .and(header("Site-Id", "3"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [{"id": 9823, "name": "Ветка 1"}]})),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    let chapters = c.ranobe_chapters("1--n").await.unwrap();
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[1].number, "0.5");
    assert_eq!(chapters[0].item_number, Some(1));
    // Ветка без branch_id: её передают как None.
    assert_eq!(chapters[1].branches[0].branch_id, None);
    assert_eq!(c.ranobe_branches(1).await.unwrap()[0].id, 9823);
}

/// Живая проверка: `cargo test -p libsocial --test ranobe -- --ignored`.
#[tokio::test]
#[ignore]
async fn live_both_content_kinds() {
    let c = Client::new(ClientConfig {
        interface: std::env::var("POLKA_INTERFACE").ok(),
        api_interval: Duration::from_millis(1100),
        ..Default::default()
    })
    .unwrap();
    let slug = "14841--beginning-after-the-end-novel";
    let old = c.ranobe_chapter(slug, "1", "0", Some(9824)).await.unwrap();
    assert_eq!(old.format, ContentFormat::Html);
    assert!(old.paragraph_count() > 5);
    let url = old.image_urls().into_iter().next().expect("в прологе есть картинка");
    assert!(url.contains("/the-beginning-after-the-end-novel/"), "{url}");
    let bytes = c.fetch_bytes(libsocial::Site::MangaLib, &url).await.unwrap();
    assert!(bytes.len() > 10_000);

    let new = c.ranobe_chapter(slug, "14", "531", Some(9824)).await.unwrap();
    assert_eq!(new.format, ContentFormat::ProseMirror);
    assert!(new.paragraph_count() > 20);
    assert!(new.image_urls().iter().all(|u| u.contains("/uploads/ranobe/14841/")));
    assert_eq!(new.doc.blocks.len(), new.spans.len());
}
