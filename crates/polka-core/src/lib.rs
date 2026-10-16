//! Ядро Полки: всё, что не зависит от интерфейса. База, загрузчик, библиотека на
//! диске, синхронизация с аккаунтом, уведомления и сообщения, запуск плеера, вход.
//! Интерфейс (сейчас iced, дальше Slint) только показывает данные ядра и передаёт
//! ему действия.

pub mod catalog;
pub mod comments;
pub mod config;
mod context;
pub mod db;
pub mod downloads;
pub mod inbox;
pub mod library;
pub mod player;
pub mod reader;
pub mod secrets;
pub mod session;
pub mod shelf;
pub mod sync;
pub mod title;
pub mod titles;

pub use context::Core;
