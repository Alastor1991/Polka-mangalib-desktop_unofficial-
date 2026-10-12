mod config;
mod login_window;
mod secrets;
mod session;

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
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        Some(other) => {
            eprintln!("неизвестная команда: {other}\n");
            print_help();
            std::process::exit(2);
        }
        None => {
            eprintln!("Интерфейс ещё не готов. Доступны команды: login, logout, whoami.");
            Ok(())
        }
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
         polka whoami               показать, под кем выполнен вход\n\n\
         Переменные окружения:\n  \
         POLKA_INTERFACE  сетевой интерфейс для запросов (например enp4s0)\n  \
         POLKA_PROXY      прокси для запросов (socks5h://127.0.0.1:1080)"
    );
}
