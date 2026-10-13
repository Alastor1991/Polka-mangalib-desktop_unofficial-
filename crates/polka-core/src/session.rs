//! Клиент API вместе с сохранённым входом.

use std::sync::Arc;

use anyhow::{Context, Result};
use libsocial::Client;

use crate::config::Config;
use crate::secrets::TokenStore;

/// Создаёт клиент, который сам сохраняет новые токены, и поднимает сохранённый вход.
/// Вход сбрасывается, только если сервер явно отверг токен. Сбой сети, зеркала или
/// хранилища секретов оставляет токен на месте, клиент обновит его позже.
pub async fn connect(config: &Config) -> Result<(Client, TokenStore)> {
    let store = TokenStore::detect();
    let saver = store.clone();
    let listener: libsocial::TokenListener = Arc::new(move |token| {
        if let Err(e) = saver.save(token) {
            eprintln!("не удалось сохранить токен: {e:#}");
        }
    });
    let client = Client::new(config.client_config())?.with_token_listener(listener);
    if let Ok(last) = std::fs::read_to_string(config.last_mirror_path()) {
        client.prefer_api(last.trim());
    }
    let saved = match store.load() {
        Ok(token) => token,
        Err(e) => {
            eprintln!("не удалось прочитать сохранённый вход, работаю гостем: {e:#}");
            None
        }
    };
    if let Some(token) = saved {
        match client.restore_token(token).await {
            Ok(_) => {}
            // Клиент уже стёр токен через слушатель.
            Err(e @ libsocial::Error::RefreshRejected) => eprintln!("{e}"),
            Err(e) => eprintln!("не удалось обновить вход, попробую позже: {e}"),
        }
    }
    Ok((client, store))
}

/// Запоминает зеркало API, которое сработало, чтобы в следующий раз начать с него.
pub fn remember_mirror(config: &Config, client: &Client) {
    let _ = crate::config::write_atomic(&config.last_mirror_path(), client.api_base().as_bytes());
}

/// Чем кончился вход.
pub struct LoggedIn {
    pub me: libsocial::models::Me,
    /// Токен удалось сохранить: после перезапуска входить заново не придётся.
    pub persisted: bool,
}

/// Меняет код из адреса возврата на токен, проверяет вход и запоминает зеркало.
pub async fn complete_login(
    config: &Config,
    client: &Client,
    store: &TokenStore,
    session: &libsocial::AuthSession,
    code: &str,
) -> Result<LoggedIn> {
    client
        .exchange_code(session, code)
        .await
        .context("обмен кода на токен")?;
    let me = client.me().await.context("проверка входа")?;
    remember_mirror(config, client);
    // Слушатель пишет токен сам, но его ошибку отсюда не видно: проверяем чтением.
    let persisted = store.load().ok().flatten() == client.token().await;
    Ok(LoggedIn { me, persisted })
}
