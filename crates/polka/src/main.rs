mod cli;
mod config;
mod db;
mod doctor;
mod downloads;
mod library;
mod login_window;
mod player;
mod secrets;
mod session;
mod sync;
mod ui;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Runtime::new()?;
    match args.first().map(String::as_str) {
        Some("login") => {
            let mode = if args.iter().any(|a| a == "--browser") {
                session::LoginMode::Browser
            } else {
                let mirror = args
                    .iter()
                    .position(|a| a == "--mirror")
                    .and_then(|i| args.get(i + 1))
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(1);
                session::LoginMode::Window {
                    mirror: mirror.saturating_sub(1),
                }
            };
            session::cli_login(&runtime, mode)
        }
        Some("logout") => runtime.block_on(session::cli_logout()),
        Some("whoami") => runtime.block_on(session::cli_whoami()),
        Some("doctor") => runtime.block_on(doctor::run()),
        Some("download") => runtime.block_on(cli::download(&args[1..])),
        // Отладка: сырой ответ API, `polka dev-get animelib episodes/123`.
        Some("dev-get") => runtime.block_on(doctor::dev_get(&args[1..])),
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        Some(other) => {
            eprintln!("неизвестная команда: {other}\n");
            print_help();
            std::process::exit(2);
        }
        None => gui(runtime),
    }
}

fn print_help() {
    println!(
        "Полка – неофициальный клиент MangaLib и AnimeLib\n\n\
         polka                      открыть приложение\n\
         polka login                войти в аккаунт lib.social в окне приложения\n\
         polka login --mirror 2     то же через запасной сервер авторизации\n\
         polka login --browser      войти через системный браузер\n\
         polka logout               выйти и удалить сохранённый токен\n\
         polka whoami               показать, под кем выполнен вход\n\
         polka doctor               проверить доступ к API, картинкам и видео\n\
         polka download <сайт> <slug> [от-до]  скачать главы или серии\n\n\
         Переменные окружения:\n  \
         POLKA_INTERFACE  сетевой интерфейс для запросов (например enp4s0)\n  \
         POLKA_PROXY      прокси для запросов (socks5h://127.0.0.1:1080)"
    );
}

/// Запускает окно приложения. Свой рантайм tokio живёт всё время работы окна:
/// на нём крутится загрузчик, а iced исполняет задачи интерфейса на своём.
fn gui(runtime: tokio::runtime::Runtime) -> Result<()> {
    let _guard = runtime.enter();
    let config = config::Config::load()?;
    let (client, store) = runtime.block_on(session::connect(&config))?;
    let db = db::Db::open(&config.db_path())?;
    let library = library::Library::new(config.library_dir());
    let downloader = downloads::Downloader::start(
        client.clone(),
        db.clone(),
        library.clone(),
        config.download_concurrency,
        config.image_server.clone(),
        &config.lock_path(),
    );
    let sync = sync::Sync::start(client.clone(), db.clone());
    let ctx = ui::Ctx {
        client,
        db,
        library,
        downloader,
        sync,
        config,
        store,
        user_id: None,
        folders: Vec::new(),
    };
    ui::run(ctx).map_err(|e| anyhow::anyhow!("интерфейс: {e}"))
}
