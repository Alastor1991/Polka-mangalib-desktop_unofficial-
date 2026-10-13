//! Общие правила для тайтлов и глав, которые нужны и интерфейсу, и командной строке.

use libsocial::Site;
use libsocial::models::{Chapter, TitleDetails};

use crate::db::TitleRow;

/// Строка базы по полной карточке тайтла.
pub fn title_row(site: Site, details: &TitleDetails) -> TitleRow {
    TitleRow {
        site,
        slug: details.short.slug_url.clone(),
        title_id: details.short.id,
        name: details.short.display_name().to_string(),
        cover_url: details.short.cover.best().map(str::to_string),
        details: serde_json::to_string(details).ok(),
        in_library: false,
    }
}

/// Ветка перевода для главы: выбранная на сайте по умолчанию, если она есть у главы.
pub fn pick_branch(chapter: &Chapter, preferred: Option<u64>) -> Option<u64> {
    chapter
        .branches
        .iter()
        .find(|b| preferred.is_some() && b.branch_id == preferred)
        .or(chapter.branches.first())
        .and_then(|b| b.branch_id)
}
