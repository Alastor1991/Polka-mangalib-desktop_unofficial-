//! Клиент API вместе с сохранённым входом.

use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use libsocial::Client;

use crate::config::Config;
use crate::secrets::TokenStore;

/// Создаёт клиент, который сам сохраняет новые токены, и поднимает сохранённый вход.
/// Если сохранённый токен уже не обновить, вход сбрасывается и клиент работает гостем.
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
    if let Some(token) = store.load()? {
        match client.restore_token(token).await {
            Ok(_) => {}
            Err(libsocial::Error::Network(e)) => {
                // Сети нет: токен не трогаем, обновится при следующем запросе.
                eprintln!("нет сети, вход проверю позже: {e}");
            }
            Err(e) => {
                eprintln!("сохранённый вход больше не действует: {e}");
                store.save(None)?;
            }
        }
    }
    Ok((client, store))
}

/// Запоминает зеркало API, которое сработало, чтобы в следующий раз начать с него.
pub fn remember_mirror(config: &Config, client: &Client) {
    let _ = crate::config::write_atomic(&config.last_mirror_path(), client.api_base().as_bytes());
}

pub async fn cli_login() -> Result<()> {
    let config = Config::load()?;
    let (client, store) = connect(&config).await?;
    let session = client.begin_login();
    let (first, rest) = session
        .authorize_urls
        .split_first()
        .context("не задано ни одного сервера авторизации")?;

    println!("Откройте адрес и войдите в аккаунт lib.social:\n\n{first}\n");
    for url in rest {
        println!("Если страница не открывается, то же самое через зеркало:\n\n{url}\n");
    }
    if open::that_detached(first).is_ok() {
        println!("Первый адрес уже открыт в браузере.");
    }
    println!(
        "После входа и подтверждения доступа сайт перекинет на страницу\n{}?code=…\n\
         Даже если она покажет ошибку, скопируйте адрес из строки браузера целиком и вставьте сюда:",
        session.redirect_uri()
    );
    print!("> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;

    let code = session.extract_code(&line)?;
    client
        .exchange_code(&session, &code)
        .await
        .context("обмен кода на токен")?;
    let me = client.me().await.context("проверка входа")?;
    remember_mirror(&config, &client);
    println!(
        "Готово, вы вошли как {} (id {}). Токен сохранён: {}.",
        me.username,
        me.id,
        store.describe()
    );
    Ok(())
}

pub async fn cli_logout() -> Result<()> {
    let store = TokenStore::detect();
    store.save(None)?;
    println!("Вход сброшен ({}).", store.describe());
    Ok(())
}

pub async fn cli_whoami() -> Result<()> {
    let config = Config::load()?;
    let (client, store) = connect(&config).await?;
    if !client.is_logged_in().await {
        println!("Вход не выполнен. Запустите `polka login`.");
        return Ok(());
    }
    let me = client.me().await?;
    remember_mirror(&config, &client);
    let token = client.token().await.context("токен пропал")?;
    let left = token.expires_at.saturating_sub(now_secs());
    println!(
        "{} (id {}), токен в: {}, действует ещё {} мин., API: {}",
        me.username,
        me.id,
        store.describe(),
        left / 60,
        client.api_base()
    );
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
