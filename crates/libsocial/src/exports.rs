//! Серверный экспорт книг ранобэ (`/book-exports`): сервер собирает EPUB или FB2 из
//! глав тайтла, клиент создаёт задачу, опрашивает её и скачивает готовый файл.
//!
//! Всё здесь написано по скриптам сайта, вживую не проверено: экспорт доступен только
//! подписчикам Premium (`bookExportPremiumOnly`), а аккаунт для проверок обычный.
//! Точной схемы ответов нет, поэтому разбор терпимый: тело берётся из `data`, а если
//! обёртки нет, то целиком. Что известно наверняка: `GET /constants` и список задач.
//!
//! Что надо знать:
//!
//! - без подписки сайт не создаёт задачу вовсе, проверяя `ExportSettings::premium_only`.
//!   Что ответит сервер на запрос без подписки, неизвестно, ожидайте 403 или 422;
//! - готовый файл отдаётся двумя способами. При `delivery == "segments"` файл режется на
//!   сегменты, которые качаются по порядку и склеиваются. Иначе для каждой готовой части
//!   нужна временная ссылка `POST .../link?part=N`, по ней идёт обычный GET;
//! - у сегмента ключ `v` в запросе обязателен, 410 значит, что файл устарел и задачу
//!   надо создавать заново, 404 – что такого сегмента нет;
//! - опрашивать надо не чаще `poll_after` секунд, по умолчанию 5. 404 на опросе значит,
//!   что задачу удалили;
//! - все запросы идут с `Site-Id: 3`.

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::ranobe::RANOBE_ORIGIN;
use crate::site::Site;

/// Формат книги. Диалог на сайте знает три, а какие включены, говорит `ExportSettings::formats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExportFormat {
    Epub,
    Fb2,
    Mobi,
}

impl ExportFormat {
    /// Значение для поля `format` запроса.
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Epub => "epub",
            ExportFormat::Fb2 => "fb2",
            ExportFormat::Mobi => "mobi",
        }
    }

    pub fn extension(self) -> &'static str {
        self.as_str()
    }
}

/// Что включено на сервере. Спрашивается у `/constants`, вход не нужен.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportSettings {
    #[serde(default, rename = "bookExportEnabled")]
    pub enabled: bool,
    /// Без подписки Premium сайт задачу не создаёт.
    #[serde(default, rename = "bookExportPremiumOnly")]
    pub premium_only: bool,
    /// Включённые форматы: `epub`, `fb2`.
    #[serde(default, rename = "bookExportFormats")]
    pub formats: Vec<String>,
}

impl ExportSettings {
    pub fn supports(&self, format: ExportFormat) -> bool {
        self.formats.iter().any(|f| f == format.as_str())
    }
}

/// Что собирать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    /// Числовой id тайтла (`RanobeChapter::manga_id`, `TitleShort::id`).
    pub manga_id: u64,
    pub format: ExportFormat,
    /// Ветка перевода. `None` и `0` сервер понимает как «любая».
    pub branch_id: Option<u64>,
    /// Один том или `None` для всех.
    pub volume: Option<String>,
}

impl ExportRequest {
    fn body(&self) -> Value {
        json!({
            "manga_id": self.manga_id,
            "format": self.format.as_str(),
            "branch_id": self.branch_id.filter(|b| *b != 0),
            "volume": self.volume,
        })
    }
}

/// Состояние задачи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportStatus {
    Queued,
    Processing,
    /// Идёт сборка файла.
    Converting,
    Completed,
    Failed,
    Cancelled,
    Expired,
    /// Статус, которого клиент не знает. Его считают незаконченным.
    Other(String),
}

impl ExportStatus {
    fn parse(s: &str) -> ExportStatus {
        match s {
            "queued" => ExportStatus::Queued,
            "processing" => ExportStatus::Processing,
            "converting" => ExportStatus::Converting,
            "completed" => ExportStatus::Completed,
            "failed" => ExportStatus::Failed,
            "cancelled" => ExportStatus::Cancelled,
            "expired" => ExportStatus::Expired,
            other => ExportStatus::Other(other.to_string()),
        }
    }

    /// Задача кончилась: готова или нет. Дальше её опрашивать незачем.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ExportStatus::Completed | ExportStatus::Failed | ExportStatus::Cancelled | ExportStatus::Expired
        )
    }
}

impl<'de> Deserialize<'de> for ExportStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => ExportStatus::parse(&s),
            _ => ExportStatus::Other(String::new()),
        })
    }
}

/// Часть готовой книги. Большая книга делится на несколько частей.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExportPart {
    #[serde(default, deserialize_with = "crate::serde_util::u64_lenient")]
    pub index: u64,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    pub name: Option<String>,
}

/// Сегмент файла при доставке `segments`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExportSegment {
    /// Значение параметра `v` при скачивании.
    #[serde(default, deserialize_with = "crate::serde_util::string_lenient")]
    pub key: String,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, deserialize_with = "crate::serde_util::u64_lenient")]
    pub size: u64,
}

/// Итоговый файл задачи.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExportFileInfo {
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::u64_lenient")]
    pub size: u64,
}

/// Задача экспорта.
#[derive(Debug, Clone, Deserialize)]
pub struct BookExport {
    /// Идентификатор задачи. Тип на сервере неизвестен, поэтому строка.
    #[serde(deserialize_with = "crate::serde_util::string_lenient")]
    pub id: String,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub manga_id: Option<u64>,
    pub status: ExportStatus,
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    pub format: Option<String>,
    /// Проценты, 0..=100.
    #[serde(default, deserialize_with = "crate::serde_util::f32_lenient")]
    pub progress: f32,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub eta_seconds: Option<u64>,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub queue_position: Option<u64>,
    #[serde(default, deserialize_with = "crate::serde_util::null_default")]
    pub queue_stalled: bool,
    /// Через сколько секунд спрашивать снова. Без значения сайт ждёт 5.
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    poll_after: Option<u64>,
    #[serde(default)]
    error: Option<ErrorBody>,
    /// `segments` или другое значение, тогда доставка по ссылкам на части.
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    pub delivery: Option<String>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub parts: Vec<ExportPart>,
    #[serde(default, deserialize_with = "crate::serde_util::lenient_vec")]
    pub segments: Vec<ExportSegment>,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub segments_total: Option<u64>,
    #[serde(default)]
    pub file: Option<ExportFileInfo>,
    /// `{format, branch_id, scope: "all"|"volumes", volumes: [...]}`.
    #[serde(default)]
    pub options: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ErrorBody {
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    message: Option<String>,
}

impl BookExport {
    /// Через сколько спрашивать снова. Не больше минуты, чтобы мусор в поле не усыпил опрос.
    pub fn poll_delay(&self) -> Duration {
        Duration::from_secs(self.poll_after.unwrap_or(5).min(60))
    }

    /// Причина, по которой задача не удалась.
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_ref()?.message.as_deref()
    }

    pub fn is_completed(&self) -> bool {
        self.status == ExportStatus::Completed
    }

    pub fn is_segmented(&self) -> bool {
        self.delivery.as_deref() == Some("segments")
    }
}

/// Временная ссылка на часть готовой книги.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportLink {
    pub url: String,
    #[serde(default, deserialize_with = "crate::serde_util::opt_string_lenient")]
    pub file_name: Option<String>,
}

/// Скачанный файл книги.
#[derive(Debug, Clone)]
pub struct DownloadedBook {
    pub name: String,
    pub bytes: Bytes,
}

/// Пауза между частями книги, как у сайта.
const PART_PAUSE: Duration = Duration::from_millis(500);

impl Client {
    /// Что включено на сервере: формы экспорта, Premium, форматы.
    pub async fn book_export_settings(&self) -> Result<ExportSettings> {
        let query = [
            ("fields[]", "bookExportEnabled"),
            ("fields[]", "bookExportPremiumOnly"),
            ("fields[]", "bookExportFormats"),
        ];
        self.get_data(Site::MangaLib, "constants", &query).await
    }

    /// Свои задачи по тайтлу. Сервер требует `manga_id`, без него 422.
    pub async fn book_exports(&self, manga_id: u64) -> Result<Vec<BookExport>> {
        let query = [("manga_id", manga_id.to_string())];
        let bytes = self
            .send_raw(Site::RanobeLib, Method::GET, "book-exports", &query, None::<&()>)
            .await?
            .bytes()
            .await?;
        let body: Value = decode("book-exports", &bytes)?;
        let list = body.get("data").unwrap_or(&body);
        let items = list.as_array().cloned().unwrap_or_default();
        Ok(items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    /// Создаёт задачу. Без подписки Premium сервер, скорее всего, откажет.
    pub async fn create_book_export(&self, request: &ExportRequest) -> Result<BookExport> {
        self.ranobe_object(Method::POST, "book-exports", Some(&request.body()))
            .await
    }

    /// Состояние задачи. `Error::NotFound` значит, что задачу удалили.
    pub async fn book_export(&self, id: &str) -> Result<BookExport> {
        self.ranobe_object(Method::GET, &format!("book-exports/{id}"), None::<&()>)
            .await
    }

    /// Убирает задачу.
    pub async fn delete_book_export(&self, id: &str) -> Result<()> {
        self.send_raw(
            Site::RanobeLib,
            Method::DELETE,
            &format!("book-exports/{id}"),
            &(),
            None::<&()>,
        )
        .await
        .map(drop)
    }

    /// Временная ссылка на готовую часть, `part` – `ExportPart::index`. Файл по ней
    /// скачивается обычным GET.
    pub async fn book_export_link(&self, id: &str, part: u64) -> Result<ExportLink> {
        let path = format!("book-exports/{id}/link");
        let query = [("part", part.to_string())];
        let bytes = self
            .send_raw(Site::RanobeLib, Method::POST, &path, &query, None::<&()>)
            .await?
            .bytes()
            .await?;
        object(&path, &bytes)
    }

    /// Один сегмент файла. `index` – номер в `BookExport::segments` с нуля, `key` – его
    /// `ExportSegment::key`. 410 приходит как `Error::Http`: файл устарел.
    pub async fn book_export_segment(&self, id: &str, index: usize, key: &str) -> Result<Bytes> {
        let path = format!("book-exports/{id}/segments/{index}");
        let query = [("v", key)];
        Ok(self
            .send_raw(Site::RanobeLib, Method::GET, &path, &query, None::<&()>)
            .await?
            .bytes()
            .await?)
    }

    /// Опрашивает задачу, пока она не кончится, через `poll_after` секунд, как сайт.
    /// `on_update` получает каждое состояние, в том числе последнее. Вернётся
    /// `Error::Other`, если задача кончилась неудачей, и тоже `Error::Other`, если за
    /// `max_wait` не закончилась. Задачу при этом не удаляет.
    pub async fn wait_book_export(
        &self,
        id: &str,
        max_wait: Duration,
        mut on_update: impl FnMut(&BookExport),
    ) -> Result<BookExport> {
        let started = Instant::now();
        loop {
            let export = self.book_export(id).await?;
            on_update(&export);
            match export.status {
                ExportStatus::Completed => return Ok(export),
                ref s if s.is_terminal() => {
                    let reason = export.error_message().unwrap_or("сервер не назвал причину");
                    return Err(Error::Other(format!(
                        "экспорт книги не удался ({:?}): {reason}",
                        export.status
                    )));
                }
                _ => {}
            }
            let delay = export.poll_delay();
            if started.elapsed() + delay > max_wait {
                return Err(Error::Other(format!(
                    "экспорт книги не закончился за {} с",
                    max_wait.as_secs()
                )));
            }
            tokio::time::sleep(delay).await;
        }
    }

    /// Скачивает готовую книгу. Сегменты склеиваются в один файл и сверяются по размеру,
    /// части качаются по временным ссылкам с паузой, каждая отдельным файлом.
    pub async fn download_book_export(&self, export: &BookExport) -> Result<Vec<DownloadedBook>> {
        if !export.is_completed() {
            return Err(Error::Other("книга ещё не собрана".into()));
        }
        let default_name = || {
            let ext = export.format.as_deref().unwrap_or("epub");
            format!("book-{}.{ext}", export.id)
        };
        if export.is_segmented() {
            if export.segments.is_empty() || export.segments.iter().any(|s| !s.ready) {
                return Err(Error::Other("не все сегменты книги готовы".into()));
            }
            let mut whole = BytesMut::new();
            for (i, segment) in export.segments.iter().enumerate() {
                whole.extend_from_slice(&self.book_export_segment(&export.id, i, &segment.key).await?);
            }
            let expected: u64 = export.segments.iter().map(|s| s.size).sum();
            if expected > 0 && whole.len() as u64 != expected {
                return Err(Error::Other(format!(
                    "книга скачана не целиком: {} байт из {expected}",
                    whole.len()
                )));
            }
            let name = export
                .file
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_else(default_name);
            return Ok(vec![DownloadedBook {
                name,
                bytes: whole.freeze(),
            }]);
        }
        let ready: Vec<&ExportPart> = export.parts.iter().filter(|p| p.ready).collect();
        if ready.is_empty() {
            return Err(Error::Other("у книги нет готовых частей".into()));
        }
        let mut books = Vec::new();
        for (n, part) in ready.iter().enumerate() {
            if n > 0 {
                tokio::time::sleep(PART_PAUSE).await;
            }
            let link = self.book_export_link(&export.id, part.index).await?;
            let url = if link.url.starts_with("http") {
                link.url.clone()
            } else {
                format!("{RANOBE_ORIGIN}/{}", link.url.trim_start_matches('/'))
            };
            let bytes = self.fetch_bytes(Site::MangaLib, &url).await?;
            let name = link
                .file_name
                .or_else(|| part.name.clone())
                .unwrap_or_else(|| format!("part-{}-{}", part.index, default_name()));
            books.push(DownloadedBook { name, bytes });
        }
        Ok(books)
    }

    /// Запрос с разбором объекта, который лежит в `data` или в корне ответа.
    async fn ranobe_object<T, B>(&self, method: Method, path: &str, body: Option<&B>) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let bytes = self
            .send_raw(Site::RanobeLib, method, path, &(), body)
            .await?
            .bytes()
            .await?;
        object(path, &bytes)
    }
}

fn decode<T: DeserializeOwned>(context: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| Error::Decode {
        context: context.to_string(),
        source,
    })
}

/// Объект из `data`, а если обёртки нет, то из корня.
fn object<T: DeserializeOwned>(context: &str, bytes: &[u8]) -> Result<T> {
    let body: Value = decode(context, bytes)?;
    let inner = match body.get("data") {
        Some(data) if data.is_object() => data.clone(),
        _ => body,
    };
    serde_json::from_value(inner).map_err(|source| Error::Decode {
        context: context.to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_uses_null_for_any_branch_and_all_volumes() {
        let request = ExportRequest {
            manga_id: 14841,
            format: ExportFormat::Fb2,
            branch_id: Some(0),
            volume: None,
        };
        assert_eq!(
            request.body(),
            json!({"manga_id": 14841, "format": "fb2", "branch_id": null, "volume": null})
        );
    }

    #[test]
    fn export_parses_with_unknown_status_and_defaults() {
        let export: BookExport =
            serde_json::from_value(json!({"id": 7, "status": "weird", "progress": "42.5"})).unwrap();
        assert_eq!(export.id, "7");
        assert_eq!(export.status, ExportStatus::Other("weird".into()));
        assert!(!export.status.is_terminal());
        assert_eq!(export.poll_delay(), Duration::from_secs(5));
        assert!((export.progress - 42.5).abs() < f32::EPSILON);
    }

    #[test]
    fn object_accepts_both_shapes() {
        let wrapped: ExportLink = object("t", br#"{"data": {"url": "https://x/y", "file_name": "a.epub"}}"#).unwrap();
        let bare: ExportLink = object("t", br#"{"url": "https://x/y"}"#).unwrap();
        assert_eq!(wrapped.file_name.as_deref(), Some("a.epub"));
        assert_eq!(bare.url, "https://x/y");
    }
}
