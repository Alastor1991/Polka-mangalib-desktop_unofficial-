//! Окно входа со встроенным браузером (WebKitGTK через wry).
//!
//! Сайт lib.social принимает возврат после входа только на свою страницу callback, а её
//! скрипт сразу забирает код и уводит на главную. Поэтому вход открывается в своём окне:
//! переход на callback перехватывается до загрузки страницы, код достаётся из адреса,
//! окно закрывается.

use anyhow::{Context, Result};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

enum UserEvent {
    Callback(String),
}

/// Показывает окно входа и ждёт возврата. Возвращает адрес callback с кодом,
/// либо `None`, если пользователь закрыл окно.
pub fn run(authorize_url: &str, redirect_uri: &str) -> Result<Option<String>> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Полка – вход в lib.social")
        .with_inner_size(tao::dpi::LogicalSize::new(520.0, 760.0))
        .build(&event_loop)
        .context("не удалось открыть окно входа")?;

    let redirect = redirect_uri.to_string();
    let builder = WebViewBuilder::new()
        .with_url(authorize_url)
        .with_navigation_handler(move |url| {
            if url.starts_with(&redirect) {
                let _ = proxy.send_event(UserEvent::Callback(url));
                // Страницу сайта не грузим, иначе её скрипт израсходует код.
                return false;
            }
            true
        });

    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().context("у окна нет контейнера GTK")?;
        builder
            .build_gtk(vbox)
            .context("не удалось запустить встроенный браузер (нужен webkit2gtk-4.1)")?
    };

    let mut result = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Callback(url)) => {
                result = Some(url);
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
    Ok(result)
}
