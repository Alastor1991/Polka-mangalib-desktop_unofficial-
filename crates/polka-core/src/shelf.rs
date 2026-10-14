//! Библиотека как полка с разделами: «На устройстве» и папки закладок аккаунта.
//! Интерфейс показывает карточки, а откуда они берутся, решается здесь.

use std::path::PathBuf;

use libsocial::{Bookmark, BookmarkPage, Site};

use crate::context::Core;
use crate::db::TitleRow;

/// Тайтл на полке.
#[derive(Debug, Clone)]
pub struct ShelfItem {
    pub site: Site,
    pub slug: String,
    pub name: String,
    /// Обложка на диске, если тайтл скачивался.
    pub local_cover: Option<PathBuf>,
    pub cover_url: Option<String>,
    /// Где остановились, если сайт это знает: «Том 1 Глава 12», «3 серия».
    pub note: Option<String>,
}

/// Тайтлы на устройстве: добавленные в библиотеку без входа, с закладкой и со
/// скачанным. Работает без сети.
pub fn device(core: &Core) -> anyhow::Result<Vec<ShelfItem>> {
    Ok(core.db.library()?.iter().map(|r| local_item(core, r)).collect())
}

fn local_item(core: &Core, r: &TitleRow) -> ShelfItem {
    let local = core.library.cover_path(r.site, &r.slug);
    ShelfItem {
        site: r.site,
        slug: r.slug.clone(),
        name: r.name.clone(),
        local_cover: local.is_file().then_some(local),
        cover_url: r.cover_url.clone(),
        note: None,
    }
}

/// Сайты, на которых есть папка: у папок манги это MangaLib, SlashLib, RanobeLib и
/// HentaiLib, у аниме один AnimeLib. Список закладок сервер отдаёт по каждому сайту
/// отдельно.
pub fn folder_sites(core: &Core, folder: u64) -> Vec<Site> {
    let Some(f) = core.folders.iter().find(|f| f.id == folder) else {
        return Vec::new();
    };
    Site::ALL.into_iter().filter(|s| f.is_for(*s)).collect()
}

/// Страница папки: ответ сервера по каждому из сайтов.
pub struct FolderPage {
    pub site: Site,
    pub page: u32,
    pub result: libsocial::Result<BookmarkPage>,
}

/// Загружает указанные страницы папки по сайтам. Запросы идут по очереди: они
/// всё равно проходят через общую очередь API.
pub async fn folder_pages(core: &Core, user_id: u64, folder: u64, pages: Vec<(Site, u32)>) -> Vec<FolderPage> {
    let mut out = Vec::new();
    for (site, page) in pages {
        let result = core.client.bookmarks(site, user_id, folder, page).await;
        out.push(FolderPage { site, page, result });
    }
    out
}

/// Карточка по закладке из списка папки.
pub fn bookmark_item(site: Site, b: &Bookmark) -> Option<ShelfItem> {
    let media = b.media.as_ref()?;
    let note = b.item.as_ref().map(|item| {
        if site.is_anime() {
            format!("{} серия", item.number)
        } else {
            format!("Том {} Глава {}", item.volume, item.number)
        }
    });
    Some(ShelfItem {
        site,
        slug: media.slug_url.clone(),
        name: media.display_name().to_string(),
        local_cover: None,
        cover_url: media.cover.best().map(str::to_string),
        note,
    })
}
