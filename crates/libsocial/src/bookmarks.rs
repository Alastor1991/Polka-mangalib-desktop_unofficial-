//! Закладки аккаунта и отметки «прочитано», «просмотрено».
//!
//! Закладка у тайтла одна. В ней папка (`status`: «Читаю», «В планах» и другие,
//! у манги и аниме свои) и место, где пользователь остановился: глава или серия и
//! позиция в ней. Через это место сайт и приложения продолжают чтение на любом
//! устройстве. Отметки о прочтении хранятся отдельно, по одной на главу или серию.

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{Auth, Client, join_url};
use crate::error::Result;
use crate::listing::Page;
use crate::models::TitleShort;
use crate::serde_util::{null_default, opt_string_lenient, opt_u64_lenient, string_lenient, u64_lenient};
use crate::site::Site;

/// Папка закладок: «Читаю», «В планах», «Любимые» и созданные пользователем.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkFolder {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    /// Сайты, к которым относится папка. У манги и аниме папки разные.
    #[serde(default, deserialize_with = "null_default")]
    pub site_ids: Vec<u8>,
}

impl BookmarkFolder {
    pub fn is_for(&self, site: Site) -> bool {
        self.site_ids.contains(&site.id())
    }
}

/// Плеер, команда и тип перевода, с которыми смотрели серию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerChoice {
    /// Название плеера, как его пишет API: `Animelib`, `Kodik`.
    pub player: String,
    pub team: u64,
    pub translation_type: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookmarkMeta {
    /// Порядковый номер главы или серии в тайтле, с единицы.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub item_number: Option<u64>,
    #[serde(default)]
    pub player: Option<String>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub team: Option<u64>,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub translation_type: Option<u64>,
}

/// Глава или серия, на которой стоит закладка.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkItem {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Папка, `BookmarkFolder::id`.
    #[serde(deserialize_with = "u64_lenient")]
    pub status: u64,
    /// Глава или серия, где остановились.
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    pub item_id: Option<u64>,
    /// Позиция в ней: номер страницы у главы, `ММ:СС` у серии.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub progress: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub meta: BookmarkMeta,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Тайтл. Есть в списке закладок, в закладке одного тайтла его нет.
    #[serde(default)]
    pub media: Option<TitleShort>,
    #[serde(default)]
    pub item: Option<BookmarkItem>,
}

impl Bookmark {
    /// Когда закладку меняли в последний раз, Unix-время. `None`, если сервер не
    /// прислал дату или прислал непонятную.
    pub fn updated_secs(&self) -> Option<u64> {
        parse_time(self.updated_at.as_deref()?)
    }

    /// Плеер, с которым смотрели серию, если он записан целиком.
    pub fn player(&self) -> Option<PlayerChoice> {
        Some(PlayerChoice {
            player: self.meta.player.clone()?,
            team: self.meta.team?,
            translation_type: self.meta.translation_type?,
        })
    }
}

/// Место, где остановились: его пишут в закладку.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    /// Id главы (перевода конкретной ветки) или серии.
    pub item_id: u64,
    pub item_number: Option<u64>,
    /// Номер страницы у главы, `ММ:СС` у серии.
    pub progress: String,
    /// Только у серий.
    pub player: Option<PlayerChoice>,
}

impl Client {
    /// Папки закладок пользователя, общие для всех сайтов сети.
    pub async fn bookmark_folders(&self, user_id: u64) -> Result<Vec<BookmarkFolder>> {
        self.get_data(Site::MangaLib, &format!("bookmarks/folder/{user_id}"), &())
            .await
    }

    /// Закладки одной папки на одном сайте, по 24 на страницу. Сервер отдаёт только
    /// тайтлы того сайта, от имени которого спрашивают.
    pub async fn bookmarks(&self, site: Site, user_id: u64, folder: u64, page: u32) -> Result<Page<Bookmark>> {
        let query = [
            ("user_id", user_id.to_string()),
            ("status", folder.to_string()),
            ("page", page.max(1).to_string()),
        ];
        self.get_page(site, "bookmarks", &query).await
    }

    /// Закладка вошедшего пользователя на тайтл. `None`, если тайтла нет в закладках.
    pub async fn bookmark(&self, site: Site, slug_url: &str) -> Result<Option<Bookmark>> {
        self.get_data(site, &format!("{}/{slug_url}/bookmark", site.catalog_path()), &())
            .await
    }

    /// Кладёт тайтл в папку и, если задано, запоминает место. Если закладка уже
    /// есть, сервер её обновляет: второй закладки на тайтл не бывает. Без `place`
    /// место в закладке остаётся прежним.
    pub async fn set_bookmark(
        &self,
        site: Site,
        slug_url: &str,
        folder: u64,
        place: Option<&Place>,
    ) -> Result<Bookmark> {
        let mut bookmark = serde_json::json!({ "status": folder });
        let mut meta = serde_json::json!({});
        if let Some(place) = place {
            bookmark["item_id"] = place.item_id.into();
            bookmark["progress"] = place.progress.clone().into();
            meta["item_number"] = place.item_number.into();
            if let Some(p) = &place.player {
                meta["player"] = p.player.clone().into();
                meta["team"] = p.team.into();
                meta["translation_type"] = p.translation_type.into();
            }
        }
        let body = serde_json::json!({
            "media_type": site.catalog_path(),
            "media_slug": slug_url,
            "bookmark": bookmark,
            "meta": meta,
        });
        self.send_data(site, Method::POST, "bookmarks", &(), Some(&body)).await
    }

    /// Убирает тайтл из закладок.
    pub async fn delete_bookmark(&self, site: Site, bookmark_id: u64) -> Result<()> {
        let query = [("media_type", site.catalog_path())];
        self.send_api(Auth::User, |http, base| {
            http.delete(join_url(base, &format!("bookmarks/{bookmark_id}")))
                .headers(Self::site_headers(site))
                .query(&query)
        })
        .await
        .map(drop)
    }

    /// Отмечает главу прочитанной. `chapter_id` – id перевода конкретной ветки,
    /// как в `ChapterBranch::id` и `ChapterPages::id`.
    pub async fn mark_chapter_viewed(&self, site: Site, title_id: u64, chapter_id: u64) -> Result<()> {
        self.post_empty(site, &format!("manga/{title_id}/chapters/{chapter_id}/view"))
            .await
    }

    /// Отмечает серию просмотренной в этом плеере.
    pub async fn mark_episode_viewed(&self, anime_id: u64, player_id: u64) -> Result<()> {
        self.post_empty(Site::AnimeLib, &format!("anime/{anime_id}/players/{player_id}/view"))
            .await
    }

    async fn post_empty(&self, site: Site, path: &str) -> Result<()> {
        self.send_api(Auth::User, |http, base| {
            http.post(join_url(base, path)).headers(Self::site_headers(site))
        })
        .await
        .map(drop)
    }
}

/// Позиция в серии для закладки: `ММ:СС`, а от часа `Ч:ММ:СС`, как пишет сайт.
pub fn format_timecode(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Обратное к `format_timecode`. Принимает и голые секунды.
pub fn parse_timecode(s: &str) -> Option<u64> {
    s.trim()
        .split(':')
        .try_fold(0u64, |acc, part| Some(acc * 60 + part.trim().parse::<u64>().ok()?))
}

/// Время сервера (`2024-09-10T05:00:52.000000Z` или `2024-09-08 15:46:01`, в UTC)
/// в Unix-секунды.
fn parse_time(s: &str) -> Option<u64> {
    let digits = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (h, mi, sec) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    // Дни от 1970-01-01 по григорианскому календарю (алгоритм Хиннанта).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecodes() {
        assert_eq!(format_timecode(754), "12:34");
        assert_eq!(format_timecode(5), "00:05");
        assert_eq!(format_timecode(3723), "1:02:03");
        assert_eq!(parse_timecode("12:34"), Some(754));
        assert_eq!(parse_timecode("1:02:03"), Some(3723));
        assert_eq!(parse_timecode("20"), Some(20));
        assert_eq!(parse_timecode("ab"), None);
    }

    #[test]
    fn server_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00.000000Z"), Some(0));
        assert_eq!(parse_time("2024-09-10T05:00:52.000000Z"), Some(1_725_944_452));
        assert_eq!(parse_time("2024-09-08 15:46:01"), Some(1_725_810_361));
        assert_eq!(parse_time("вчера"), None);
    }
}
