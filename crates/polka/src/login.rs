//! Вход, выход и «кто я» из терминала. Окно входа живёт здесь, а не в ядре: ему
//! нужен WebKitGTK, а ядро от интерфейса не зависит.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use libsocial::Client;
use polka_core::config::Config;
use polka_core::secrets::TokenStore;
use polka_core::session;

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
    let (client, store) = runtime.block_on(session::connect(&config))?;
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
    let done = session::complete_login(config, client, store, session, code).await?;
    let me = done.me;
    if done.persisted {
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

/// Сколько `polka logout` ждёт сервер. Хранилище стирается и по истечении срока.
const LOGOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Чем кончился выход для токена на сервере.
enum Revoke {
    Revoked,
    /// Сервер отверг refresh token: просроченный токен и так ничего не открывает.
    AlreadyDead,
    Failed,
    NoLogin,
}

pub async fn cli_logout() -> Result<()> {
    let store = TokenStore::detect();
    // Выйти должно получаться и со сломанными настройками, прокси или хранилищем:
    // тогда токен только стирается на этом компьютере.
    let saved = match store.load() {
        Ok(saved) => saved,
        Err(e) => {
            eprintln!("не удалось прочитать сохранённый вход, просто стираю его: {e:#}");
            None
        }
    };
    let outcome = match saved {
        None => Revoke::NoLogin,
        Some(token) => match tokio::time::timeout(LOGOUT_WAIT, revoke(token)).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => {
                eprintln!("не удалось связаться с сервером для выхода: {e:#}");
                Revoke::Failed
            }
            Err(_) => {
                eprintln!("сервер не ответил за {} с", LOGOUT_WAIT.as_secs());
                Revoke::Failed
            }
        },
    };
    store.save(None)?;
    let place = store.describe();
    match outcome {
        Revoke::Revoked => println!("Вы вышли, токен отозван на сервере и стёрт ({place})."),
        Revoke::AlreadyDead => println!("Вход уже истёк на сервере, токен стёрт ({place})."),
        Revoke::Failed => println!(
            "Вход сброшен ({place}). Отозвать токен на сервере не получилось: \
             он перестанет действовать сам, когда истечёт."
        ),
        Revoke::NoLogin => println!("Вход и так не был выполнен ({place})."),
    }
    Ok(())
}

/// Отзывает токен из хранилища. Клиент без слушателя: хранилище стирает `cli_logout`,
/// а `session::connect` печатал бы сообщения о входе, неуместные при выходе.
async fn revoke(token: libsocial::Token) -> Result<Revoke> {
    let client = session::new_client(&Config::load()?)?;
    // Просроченный токен сервер отзывать не станет и ответит 401, поэтому его сначала
    // обновляем, как при запуске, и отзываем уже свежий.
    // Прочие сбои обновления не мешают: попробуем отозвать то, что есть.
    if let Err(libsocial::Error::RefreshRejected) = client.restore_token(token).await {
        return Ok(Revoke::AlreadyDead);
    }
    Ok(if client.logout().await {
        Revoke::Revoked
    } else {
        Revoke::Failed
    })
}

pub async fn cli_whoami() -> Result<()> {
    let config = Config::load()?;
    let (client, store) = session::connect(&config).await?;
    if !client.is_logged_in().await {
        println!("Вход не выполнен. Запустите `polka login`.");
        return Ok(());
    }
    let me = client.me().await?;
    session::remember_mirror(&config, &client);
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
