//! Неофициальный клиент API сервисов lib.social: MangaLib, SlashLib, RanobeLib,
//! HentaiLib, AnimeLib.
//!
//! Крейт ничего не знает об интерфейсе и хранилище. Он умеет ходить в API аккуратно
//! (ограничение частоты, повторы, обновление токена, зеркала) и разбирать ответы в
//! типы. Каждая область сервиса – свой модуль: каталог и поиск (`catalog`, `feeds`,
//! `search`), тайтл и главы (`manga`, `anime`, `media`, `ranobe`), закладки
//! (`bookmarks`, `bookmarks_ext`), комментарии (`comments`), аккаунт и общение
//! (`notifications`, `users`, `social`, `collections`), карточки (`cards`), экспорт
//! книг (`exports`). Форматированный текст сервиса разбирает `rich`. Карта модулей и
//! правила – в `docs/architecture.md`, протокол – в `docs/api.md`.

mod anime;
pub mod auth;
pub mod bookmarks;
pub mod bookmarks_ext;
pub mod cards;
pub mod catalog;
pub mod catalog_constants;
mod client;
pub mod collections;
pub mod comments;
mod error;
pub mod exports;
pub mod feeds;
pub mod listing;
mod manga;
pub mod media;
pub mod mirrors;
pub mod models;
pub mod notifications;
pub mod ranobe;
pub mod rich;
pub mod search;
mod serde_util;
mod site;
pub mod social;
pub mod users;

pub use anime::{ResolvedSubtitle, ResolvedVideo};
pub use auth::{AuthSession, Token};
pub use bookmarks::{Bookmark, BookmarkFolder, Place, PlayerChoice};
pub use client::{Client, ClientConfig, TokenListener};
pub use error::{Error, Result};
pub use listing::Page;
pub use site::Site;
