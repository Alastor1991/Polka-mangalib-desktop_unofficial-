//! Неофициальный клиент API сервисов lib.social: MangaLib, SlashLib, HentaiLib, AnimeLib.
//!
//! Крейт ничего не знает об интерфейсе и хранилище. Он умеет ходить в API аккуратно
//! (ограничение частоты, повторы, обновление токена) и разбирать ответы в типы.

mod anime;
pub mod auth;
pub mod bookmarks;
mod client;
mod error;
mod manga;
pub mod mirrors;
pub mod models;
mod serde_util;
mod site;

pub use anime::{ResolvedSubtitle, ResolvedVideo};
pub use auth::{AuthSession, Token};
pub use bookmarks::{Bookmark, BookmarkFolder, BookmarkPage, Place, PlayerChoice};
pub use client::{Client, ClientConfig, TokenListener};
pub use error::{Error, Result};
pub use manga::{CatalogQuery, Sort};
pub use site::Site;
