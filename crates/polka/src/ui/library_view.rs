//! Библиотека: тайтлы, добавленные вручную или со скачанным. Работает без сети.

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Fill, Task};
use libsocial::Site;

use super::catalog::{CARD_H, CARD_W, cover};
use super::images::{ImageReq, Images, Loaded};
use super::{Ctx, Message};
use crate::db::TitleRow;

#[derive(Default)]
pub struct LibraryView {
    rows: Vec<TitleRow>,
    /// Обложка для каждой строки. Считается при загрузке списка, а не при каждой
    /// отрисовке: проверка файла на диске в отрисовке тормозила бы прокрутку.
    covers: Vec<Option<ImageReq>>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Open(Site, String),
    Image(String, Loaded),
}

impl LibraryView {
    pub fn reload(&mut self, ctx: &Ctx, images: &mut Images) -> Task<Message> {
        match ctx.db.library() {
            Ok(rows) => {
                self.rows = rows;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.covers = self.rows.iter().map(|r| cover_req(ctx, r)).collect();
        let tasks: Vec<_> = self
            .covers
            .iter()
            .flatten()
            .cloned()
            .map(|req| images.request(req, |k, l| Message::Library(Msg::Image(k, l))))
            .collect();
        Task::batch(tasks)
    }

    /// Возвращает тайтл, который надо открыть, или задачу размещения картинки.
    pub fn update(&mut self, msg: Msg, images: &mut Images) -> (Task<Message>, Option<(Site, String)>) {
        match msg {
            Msg::Open(site, slug) => (Task::none(), Some((site, slug))),
            Msg::Image(key, loaded) => (
                images.finish(key, loaded, |k, l| Message::Library(Msg::Image(k, l))),
                None,
            ),
        }
    }

    pub fn view<'a>(&'a self, images: &'a Images) -> Element<'a, Msg> {
        if let Some(e) = &self.error {
            return container(text(format!("Библиотека не открылась: {e}")))
                .padding(20)
                .into();
        }
        if self.rows.is_empty() {
            return container(
                text("Здесь появятся тайтлы, которые вы добавите в библиотеку или начнёте скачивать.")
                    .style(text::secondary),
            )
            .padding(20)
            .into();
        }
        let cards = row(self.rows.iter().zip(&self.covers).map(|(r, req)| {
            button(
                column![
                    cover(images, req.as_ref(), CARD_W, CARD_H),
                    text(&r.name).size(14).width(CARD_W).height(38),
                    text(r.site.name()).size(12).style(text::secondary),
                ]
                .spacing(6),
            )
            .padding(6)
            .style(button::text)
            .on_press(Msg::Open(r.site, r.slug.clone()))
            .into()
        }))
        .spacing(14)
        .wrap();
        scrollable(column![cards].padding(16).width(Fill)).height(Fill).into()
    }
}

/// Обложка с диска, если тайтл скачивался, иначе из сети.
fn cover_req(ctx: &Ctx, r: &TitleRow) -> Option<ImageReq> {
    let local = ctx.library.cover_path(r.site, &r.slug);
    if local.is_file() {
        return Some(ImageReq::File(local));
    }
    r.cover_url.as_ref().map(|url| ImageReq::Url {
        site: r.site,
        url: url.clone(),
    })
}
