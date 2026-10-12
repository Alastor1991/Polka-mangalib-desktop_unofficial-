use reqwest::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("сеть: {0}")]
    Network(#[from] reqwest::Error),

    #[error("сервер ответил {status}: {body}")]
    Http { status: StatusCode, body: String },

    #[error("доступ запрещён (403). Сервис закрыт для этого IP или региона")]
    Forbidden,

    #[error("нужен вход в аккаунт")]
    Unauthorized,

    #[error("не найдено")]
    NotFound,

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
