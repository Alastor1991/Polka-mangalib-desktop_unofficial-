//! Очередь загрузок.

use iced::widget::{button, column, container, progress_bar, row, scrollable, text};
use iced::{Alignment, Element, Fill};
use libsocial::Site;

use super::Ctx;
use crate::db::{DownloadRow, DownloadState};
use crate::downloads::DownloadEvent;

#[derive(Default)]
pub struct DownloadsView {
    rows: Vec<DownloadRow>,
    /// Названия тайтлов по (сайт, slug).
    names: std::collections::HashMap<(Site, String), String>,
    show_done: bool,
    /// Очередь сейчас выполняет другой процесс Полки.
    elsewhere: bool,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Pause(Site, String, String),
    Resume(Site, String, String),
    Remove(Site, String, String),
    ToggleDone,
    RetryFailed,
}

impl DownloadsView {
    pub fn reload(&mut self, ctx: &Ctx) {
        self.rows = ctx.db.downloads().unwrap_or_default();
        self.elsewhere = !ctx.downloader.is_active();
        for r in &self.rows {
            let key = (r.site, r.slug.clone());
            if !self.names.contains_key(&key)
                && let Ok(Some(t)) = ctx.db.title(r.site, &r.slug)
            {
                self.names.insert(key, t.name);
            }
        }
    }

    pub fn on_event(&mut self, e: &DownloadEvent, ctx: &Ctx) {
        match self
            .rows
            .iter_mut()
            .find(|r| r.site == e.site && r.slug == e.slug && r.item == e.item)
        {
            Some(_) if e.removed => self.reload(ctx),
            Some(row) if e.total > 0 || e.state != DownloadState::Running => {
                row.state = e.state;
                if e.total > 0 {
                    row.done = e.done;
                    row.total = e.total;
                }
                row.error = e.error.clone();
            }
            Some(_) => {}
            // Новое задание или удалённое: проще перечитать список.
            None => self.reload(ctx),
        }
    }

    /// Возвращает текст для всплывающего сообщения, если что-то пошло не так.
    pub fn update(&mut self, msg: Msg, ctx: &Ctx) -> Option<String> {
        let result = match &msg {
            Msg::Pause(site, slug, item) => ctx.downloader.pause(*site, slug, item),
            Msg::Resume(site, slug, item) => ctx.downloader.resume(*site, slug, item),
            Msg::Remove(site, slug, item) => ctx.downloader.remove(*site, slug, item),
            Msg::ToggleDone => {
                self.show_done = !self.show_done;
                Ok(())
            }
            Msg::RetryFailed => self
                .rows
                .iter()
                .filter(|r| r.state == DownloadState::Failed)
                .try_for_each(|r| ctx.downloader.resume(r.site, &r.slug, &r.item)),
        };
        self.reload(ctx);
        result.err().map(|e| format!("Не получилось: {e:#}"))
    }

    pub fn view(&self) -> Element<'_, Msg> {
        let active = self.rows.iter().filter(|r| r.state != DownloadState::Done).count();
        let done = self.rows.len() - active;
        let failed = self.rows.iter().filter(|r| r.state == DownloadState::Failed).count();
        let mut header = row![
            text(format!("В работе: {active}")).size(16),
            button(text(if self.show_done {
                format!("Скрыть скачанное ({done})")
            } else {
                format!("Показать скачанное ({done})")
            }))
            .style(button::text)
            .on_press(Msg::ToggleDone),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if failed > 0 {
            header = header.push(button(text(format!("Повторить упавшие ({failed})"))).on_press(Msg::RetryFailed));
        }

        let rows = self
            .rows
            .iter()
            .filter(|r| self.show_done || r.state != DownloadState::Done)
            .map(|r| self.row_view(r));
        let list = column(rows).spacing(6);
        let mut header = column![header];
        if self.elsewhere {
            header = header.push(
                text("Загрузки сейчас ведёт другой запуск Полки (например, polka download). Когда он закончит, очередь продолжится здесь.")
                    .size(13)
                    .style(text::secondary),
            );
        }
        let body: Element<'_, Msg> = if self.rows.is_empty() {
            text("Очередь пуста. Главы и серии ставятся в очередь со страницы тайтла.")
                .style(text::secondary)
                .into()
        } else {
            scrollable(list).height(Fill).into()
        };
        column![header, body].spacing(14).padding(16).into()
    }

    fn row_view<'a>(&'a self, r: &'a DownloadRow) -> Element<'a, Msg> {
        let name = self
            .names
            .get(&(r.site, r.slug.clone()))
            .map(String::as_str)
            .unwrap_or(&r.slug);
        let video = r.item.starts_with('e');
        let progress = if r.total > 0 {
            r.done as f32 / r.total as f32
        } else {
            0.0
        };
        let amount = match (r.total, video) {
            (0, _) => String::new(),
            (t, true) => format!("{} / {} МБ", r.done >> 20, t >> 20),
            (t, false) => format!("{} / {} стр.", r.done, t),
        };
        let state = match r.state {
            DownloadState::Queued => "в очереди",
            DownloadState::Running => "качается",
            DownloadState::Paused => "пауза",
            DownloadState::Done => "готово",
            DownloadState::Failed => "ошибка",
        };
        let id = (r.site, r.slug.clone(), r.item.clone());
        let action = match r.state {
            DownloadState::Running | DownloadState::Queued => Some(
                button("Пауза")
                    .style(button::secondary)
                    .on_press(Msg::Pause(id.0, id.1.clone(), id.2.clone())),
            ),
            DownloadState::Paused | DownloadState::Failed => {
                Some(button("Продолжить").on_press(Msg::Resume(id.0, id.1.clone(), id.2.clone())))
            }
            DownloadState::Done => None,
        };
        let mut line = row![
            column![
                text(format!("{name} – {}", r.label)).size(14),
                text(match &r.error {
                    Some(e) => format!("{state}: {e}"),
                    None => format!("{state} {amount}"),
                })
                .size(12)
                .style(if r.error.is_some() {
                    text::danger
                } else {
                    text::secondary
                }),
                progress_bar(0.0..=1.0, progress).girth(4),
            ]
            .spacing(4)
            .width(Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(action) = action {
            line = line.push(action);
        }
        line = line.push(
            button("Удалить")
                .style(button::danger)
                .on_press(Msg::Remove(id.0, id.1, id.2)),
        );
        container(line).padding([8, 12]).style(super::style::card).into()
    }
}
