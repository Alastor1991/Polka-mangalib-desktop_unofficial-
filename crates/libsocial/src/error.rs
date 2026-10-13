use std::time::Duration;

use reqwest::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("сеть: {0}")]
    Network(#[from] reqwest::Error),

    #[error("сервер ответил {status}: {body}")]
    Http { status: StatusCode, body: String },

    #[error("доступ запрещён (403) или все зеркала закрыты для этой сети")]
    Forbidden,

    #[error("нужен вход в аккаунт")]
    Unauthorized,

    /// Сервер явно отверг refresh token: вход надо пройти заново.
    #[error("сохранённый вход больше не действует, войдите заново")]
    RefreshRejected,

    /// Сервер не умеет отдавать файл с середины: ответил целиком вместо куска.
    #[error("сервер не поддерживает докачку")]
    RangeUnsupported,

    /// Запрошенный кусок за концом файла: обычно значит, что файл уже скачан целиком.
    #[error("запрошенный кусок за концом файла")]
    RangeNotSatisfiable,

    #[error("не найдено")]
    NotFound,

    /// Сервис попросил подождать дольше, чем разумно ждать внутри одного запроса.
    /// До конца паузы клиент к API не ходит.
    #[error("сервис просит подождать {} мин.", .retry_after.as_secs().div_ceil(60).max(1))]
    RateLimited { retry_after: Duration },

    /// Шлюз перед сервисом отвечает ошибкой (502–504, 52x), причём на всех зеркалах
    /// сразу: лежит сам сервис, а не одно зеркало.
    #[error("сервис lib.social временно не отвечает ({status}), попробуйте позже")]
    ServiceDown { status: StatusCode },

    #[error("не удалось разобрать ответ {context}: {source}")]
    Decode {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("вход: {0}")]
    Auth(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
