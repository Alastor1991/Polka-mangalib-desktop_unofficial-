//! Общие стили поверх темы.

use iced::widget::container;
use iced::{Border, Theme};

/// Верхняя панель.
pub fn bar(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.weak.color.into()),
        ..container::Style::default()
    }
}

/// Карточка тайтла в сетке и строки списков.
pub fn card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.weak.color.into()),
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Заглушка на месте картинки, пока она грузится.
pub fn placeholder(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.strong.color.into()),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Плашка с сообщением внизу окна.
pub fn toast(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.primary.weak.color.into()),
        text_color: Some(palette.primary.weak.text),
        ..container::Style::default()
    }
}

/// Фон читалки: тёмный, чтобы страницы не слепили.
pub fn reader_bg(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Color::from_rgb8(0x10, 0x10, 0x14).into()),
        text_color: Some(iced::Color::from_rgb8(0xdd, 0xdd, 0xe4)),
        ..container::Style::default()
    }
}
