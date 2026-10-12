mod config;
mod secrets;
mod session;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Runtime::new()?;
    match args.first().map(String::as_str) {
        Some("login") => runtime.block_on(session::cli_login()),
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
         polka           открыть приложение\n\
         polka login     войти в аккаунт lib.social через браузер\n\
         polka logout    выйти и удалить сохранённый токен\n\
         polka whoami    показать, под кем выполнен вход\n\n\
         Переменные окружения:\n  \
         POLKA_INTERFACE  сетевой интерфейс для запросов (например enp4s0)\n  \
         POLKA_PROXY      прокси для запросов (socks5h://127.0.0.1:1080)"
    );
}
