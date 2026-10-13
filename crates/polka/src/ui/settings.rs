//! Настройки и вход.

use iced::widget::{button, column, pick_list, row, text, text_input};
use iced::{Alignment, Element, Task};
use libsocial::models::Me;

use super::{Ctx, Message};
use crate::config::Config;

const SERVERS: [&str; 4] = ["compress", "main", "secondary", "download"];
const QUALITIES: [u64; 5] = [360, 480, 720, 1080, 2160];

pub struct Settings {
    pub me: Option<Me>,
    interface: String,
    proxy: String,
    server: String,
    quality: u64,
    concurrency: usize,
    busy: bool,
    dirty: bool,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Login,
    LoginDone(Result<Option<Me>, String>),
    Logout,
    Interface(String),
    Proxy(String),
    Server(&'static str),
    Quality(u64),
    Concurrency(usize),
    Save,
}

impl Settings {
    pub fn new(config: &Config) -> Settings {
        Settings {
            me: None,
            interface: config.interface.clone().unwrap_or_default(),
            proxy: config.proxy.clone().unwrap_or_default(),
            server: config.image_server.clone(),
            quality: config.max_quality,
            concurrency: config.download_concurrency,
            busy: false,
            dirty: false,
        }
    }

    pub fn update(&mut self, msg: Msg, ctx: &mut Ctx) -> (Task<Message>, Option<String>) {
        match msg {
            Msg::Login => {
                self.busy = true;
                let client = ctx.client.clone();
                let store = ctx.store.clone();
                let task = Task::perform(login(client, store), |r| Message::Settings(Msg::LoginDone(r)));
                (task, None)
            }
            Msg::LoginDone(result) => {
                self.busy = false;
                match result {
                    Ok(Some(me)) => {
                        let name = me.username.clone();
                        self.me = Some(me.clone());
                        (Task::done(Message::Me(Some(me))), Some(format!("Вы вошли как {name}")))
                    }
                    Ok(None) => (Task::none(), Some("Вход отменён".into())),
                    Err(e) => (Task::none(), Some(format!("Вход не удался: {e}"))),
                }
            }
            Msg::Logout => {
                let client = ctx.client.clone();
                self.me = None;
                (
                    Task::perform(async move { client.logout().await }, |_| Message::Me(None)),
                    Some("Вы вышли из аккаунта".into()),
                )
            }
            Msg::Interface(v) => self.touch(|s| s.interface = v),
            Msg::Proxy(v) => self.touch(|s| s.proxy = v),
            Msg::Server(v) => self.touch(|s| s.server = v.to_string()),
            Msg::Quality(v) => self.touch(|s| s.quality = v),
            Msg::Concurrency(v) => self.touch(|s| s.concurrency = v),
            Msg::Save => {
                let mut config = ctx.config.clone();
                config.interface = Some(self.interface.trim().to_string()).filter(|s| !s.is_empty());
                config.proxy = Some(self.proxy.trim().to_string()).filter(|s| !s.is_empty());
                config.image_server = self.server.clone();
                config.max_quality = self.quality;
                config.download_concurrency = self.concurrency;
                match config.save() {
                    Ok(()) => {
                        let network = config.interface != ctx.config.interface || config.proxy != ctx.config.proxy;
                        ctx.config = config;
                        self.dirty = false;
                        let text = if network {
                            "Сохранено. Сетевые настройки заработают после перезапуска."
                        } else {
                            "Сохранено. Сервер картинок и параллельность загрузок применятся после перезапуска."
                        };
                        (Task::none(), Some(text.into()))
                    }
                    Err(e) => (Task::none(), Some(format!("Не сохранилось: {e:#}"))),
                }
            }
        }
    }

    fn touch(&mut self, f: impl FnOnce(&mut Settings)) -> (Task<Message>, Option<String>) {
        f(self);
        self.dirty = true;
        (Task::none(), None)
    }

    pub fn view(&self, ctx: &Ctx) -> Element<'_, Msg> {
        let account: Element<'_, Msg> = match &self.me {
            Some(me) => row![
                text(format!("Вход выполнен: {} (id {})", me.username, me.id)),
                button("Выйти").style(button::danger).on_press(Msg::Logout),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                text("Вход не выполнен: без него недоступны 18+ разделы и видео AnimeLib."),
                button(if self.busy {
                    "Окно входа открыто…"
                } else {
                    "Войти"
                })
                .on_press_maybe((!self.busy).then_some(Msg::Login)),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
        };
        let field = |label: &'static str, input: Element<'static, Msg>| {
            row![text(label).width(260), input]
                .spacing(12)
                .align_y(Alignment::Center)
        };
        column![
            text("Аккаунт").size(20),
            account,
            text(format!("Токен хранится: {}", ctx.store.describe()))
                .size(13)
                .style(text::secondary),
            text("Сеть").size(20),
            field(
                "Сетевой интерфейс (необязательно)",
                text_input("например enp4s0", &self.interface)
                    .on_input(Msg::Interface)
                    .width(320)
                    .into()
            ),
            field(
                "Прокси (необязательно)",
                text_input("socks5h://127.0.0.1:1080", &self.proxy)
                    .on_input(Msg::Proxy)
                    .width(320)
                    .into()
            ),
            text(format!("Сейчас API работает через {}", ctx.client.api_base()))
                .size(13)
                .style(text::secondary),
            text("Загрузки").size(20),
            field(
                "Сервер картинок",
                pick_list(SERVERS, Some(server_static(&self.server)), Msg::Server).into()
            ),
            field(
                "Качество видео не выше",
                pick_list(QUALITIES, Some(self.quality), Msg::Quality).into()
            ),
            field(
                "Картинок одновременно",
                pick_list([1usize, 2, 3, 4, 6, 8], Some(self.concurrency), Msg::Concurrency).into()
            ),
            text(format!("Библиотека: {}", ctx.library.root().display()))
                .size(13)
                .style(text::secondary),
            button("Сохранить").on_press_maybe(self.dirty.then_some(Msg::Save)),
        ]
        .spacing(12)
        .padding(20)
        .into()
    }
}

fn server_static(s: &str) -> &'static str {
    SERVERS.into_iter().find(|v| *v == s).unwrap_or("compress")
}

/// Вход в отдельном процессе: окну WebKitGTK нужен главный поток, а он занят iced.
/// Новый токен процесс кладёт в хранилище, отсюда его остаётся прочитать.
async fn login(client: libsocial::Client, store: crate::secrets::TokenStore) -> Result<Option<Me>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = tokio::process::Command::new(exe)
        .arg("login")
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("окно входа завершилось с ошибкой".into());
    }
    let token = store.load().map_err(|e| format!("{e:#}"))?;
    let Some(token) = token else { return Ok(None) };
    client.restore_token(token).await.map_err(|e| e.to_string())?;
    client.me().await.map(Some).map_err(|e| e.to_string())
}
