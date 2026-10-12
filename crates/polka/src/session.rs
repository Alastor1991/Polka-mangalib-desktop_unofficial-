//! Клиент API вместе с сохранённым входом.

use std::io::{BufRead, Write};
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

/// Как пройти вход.
pub enum LoginMode {
    /// Своё окно со встроенным браузером, код перехватывается сам.
    Window { mirror: usize },
    /// Системный браузер, адрес с кодом пользователь вставляет в терминал.
    Browser,
}

/// Вход из терминала. Окно входа живёт в главном потоке (так требует GTK),
/// поэтому асинхронные шаги идут через `runtime.block_on`.
pub fn cli_login(runtime: &tokio::runtime::Runtime, mode: LoginMode) -> Result<()> {
    let config = Config::load()?;
    let (client, store) = runtime.block_on(connect(&config))?;
    let session = client.begin_login();
    if session.authorize_urls.is_empty() {
        anyhow::bail!("не задано ни одного сервера авторизации");
    }

    let pasted = match mode {
        LoginMode::Window { mirror } => {
            let idx = mirror.min(session.authorize_urls.len() - 1);
            println!("Открываю окно входа ({}).", host(&session.authorize_urls[idx]));
            match crate::login_window::run(&session.authorize_urls[idx], session.redirect_uri())? {
                Some(url) => url,
                None => {
                    let next = idx + 2;
                    println!("Окно закрыто до входа.");
                    if next <= session.authorize_urls.len() {
                        println!("Если страница входа не открылась, попробуйте зеркало: polka login --mirror {next}");
                    }
                    println!("Или вход через системный браузер: polka login --browser");
                    return Ok(());
                }
            }
        }
        LoginMode::Browser => read_callback_from_terminal(&session)?,
    };

    let code = session.extract_code(&pasted)?;
    runtime.block_on(finish_login(&config, &client, &store, &session, &code))
}

fn read_callback_from_terminal(session: &libsocial::AuthSession) -> Result<String> {
    let (first, rest) = session.authorize_urls.split_first().context("нет адреса входа")?;
    println!("Откройте адрес и войдите в аккаунт lib.social:\n\n{first}\n");
    for url in rest {
        println!("Если страница не открывается, то же самое через зеркало:\n\n{url}\n");
    }
    if open::that_detached(first).is_ok() {
        println!("Первый адрес уже открыт в браузере.");
    }
    println!(
        "После входа сайт откроет {}?code=… и сразу уйдёт на главную.\n\
         Найдите этот адрес в истории браузера (Ctrl+H, поиск «callback») и вставьте сюда:",
        session.redirect_uri()
    );
    print!("> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

fn host(url: &str) -> &str {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url)
}

async fn finish_login(
    config: &Config,
    client: &Client,
    store: &TokenStore,
    session: &libsocial::AuthSession,
    code: &str,
) -> Result<()> {
    client
        .exchange_code(session, code)
        .await
        .context("обмен кода на токен")?;
    let me = client.me().await.context("проверка входа")?;
    remember_mirror(config, client);
    // Слушатель пишет токен сам, но его ошибку отсюда не видно: проверяем чтением.
    let persisted = store.load().ok().flatten() == client.token().await;
    if persisted {
        println!(
            "Готово, вы вошли как {} (id {}). Токен сохранён: {}.",
            me.username,
            me.id,
            store.describe()
        );
    } else {
        println!(
            "Вы вошли как {} (id {}), но токен не удалось сохранить в {}. \
             После перезапуска вход придётся повторить.",
            me.username,
            me.id,
            store.describe()
        );
    }
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
