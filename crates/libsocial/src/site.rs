use serde::{Deserialize, Serialize};

/// Сайт сети lib.social. Сервер различает их по заголовку `Site-Id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Site {
    MangaLib,
    SlashLib,
    HentaiLib,
    AnimeLib,
}

impl Site {
    pub const ALL: [Site; 4] = [Site::MangaLib, Site::SlashLib, Site::HentaiLib, Site::AnimeLib];

    pub fn id(self) -> u8 {
        match self {
            Site::MangaLib => 1,
            Site::SlashLib => 2,
            Site::HentaiLib => 4,
            Site::AnimeLib => 5,
        }
    }

    pub fn from_id(id: u8) -> Option<Site> {
        Site::ALL.into_iter().find(|s| s.id() == id)
    }

    /// Домен сайта. Его же сервер ждёт в `Referer` и `Origin`.
    pub fn domain(self) -> &'static str {
        match self {
            Site::MangaLib => "mangalib.me",
            Site::SlashLib => "slashlib.me",
            Site::HentaiLib => "hentailib.me",
            Site::AnimeLib => "anilib.me",
        }
    }

    pub fn origin(self) -> String {
        format!("https://{}", self.domain())
    }

    pub fn name(self) -> &'static str {
        match self {
            Site::MangaLib => "MangaLib",
            Site::SlashLib => "SlashLib",
            Site::HentaiLib => "HentaiLib",
            Site::AnimeLib => "AnimeLib",
        }
    }

    /// Ключ для путей на диске и в базе.
    pub fn key(self) -> &'static str {
        match self {
            Site::MangaLib => "mangalib",
            Site::SlashLib => "slashlib",
            Site::HentaiLib => "hentailib",
            Site::AnimeLib => "animelib",
        }
    }

    pub fn from_key(key: &str) -> Option<Site> {
        Site::ALL.into_iter().find(|s| s.key() == key)
    }

    pub fn is_anime(self) -> bool {
        self == Site::AnimeLib
    }

    /// Раздел каталога в API: `manga` или `anime`.
    pub(crate) fn catalog_path(self) -> &'static str {
        if self.is_anime() { "anime" } else { "manga" }
    }

    /// Адрес тайтла на первом зеркале сайта, чтобы открыть его в браузере.
    pub fn title_url(self, slug_url: &str) -> String {
        format!(
            "{}/ru/{}/{}",
            crate::mirrors::web(self)[0],
            self.catalog_path(),
            slug_url
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        for site in Site::ALL {
            assert_eq!(Site::from_id(site.id()), Some(site));
            assert_eq!(Site::from_key(site.key()), Some(site));
        }
    }
}
