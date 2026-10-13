//! Типы ответов API. Поля, которые клиенту не нужны, не описаны: serde их пропускает.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serde_util::{f32_lenient, null_default, string_lenient, u64_lenient};
use crate::site::Site;

/// Почти все ответы завёрнуты в `{"data": ...}`.
#[derive(Debug, Deserialize)]
pub(crate) struct Data<T> {
    pub data: T,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMeta {
    #[serde(default)]
    pub has_next_page: bool,
    #[serde(default)]
    pub current_page: Option<u32>,
}

/// Страница каталога.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paged<T> {
    /// Элемент, который не удалось разобрать, пропускается, а не ломает всю страницу.
    #[serde(
        deserialize_with = "crate::serde_util::lenient_vec",
        bound(deserialize = "T: serde::de::DeserializeOwned")
    )]
    pub data: Vec<T>,
    #[serde(default)]
    pub meta: PageMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cover {
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub md: Option<String>,
}

impl Cover {
    /// Лучшая доступная картинка для карточки.
    pub fn best(&self) -> Option<&str> {
        self.default
            .as_deref()
            .or(self.md.as_deref())
            .or(self.thumbnail.as_deref())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Label {
    #[serde(default, deserialize_with = "string_lenient")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Named {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rating {
    #[serde(default, deserialize_with = "f32_lenient")]
    pub average: f32,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub votes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Team {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub slug_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserRef {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
}

/// Тайтл в каталоге и поиске.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleShort {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub rus_name: Option<String>,
    #[serde(default)]
    pub eng_name: Option<String>,
    pub slug_url: String,
    #[serde(default, deserialize_with = "null_default")]
    pub cover: Cover,
    #[serde(default)]
    pub site: Option<u8>,
    #[serde(default, rename = "type")]
    pub kind: Option<Label>,
    #[serde(default, rename = "ageRestriction")]
    pub age_restriction: Option<Label>,
    #[serde(default)]
    pub status: Option<Label>,
    #[serde(default)]
    pub rating: Option<Rating>,
    #[serde(default, rename = "releaseDateString")]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_licensed: bool,
}

impl TitleShort {
    /// Русское название, если есть, иначе оригинальное.
    pub fn display_name(&self) -> &str {
        self.rus_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    pub fn site(&self) -> Option<Site> {
        self.site.and_then(Site::from_id)
    }
}

/// Полная карточка тайтла.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleDetails {
    #[serde(flatten)]
    pub short: TitleShort,
    #[serde(default)]
    pub summary: Option<Value>,
    #[serde(default, rename = "otherNames", deserialize_with = "null_default")]
    pub other_names: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub genres: Vec<Named>,
    #[serde(default, deserialize_with = "null_default")]
    pub tags: Vec<Named>,
    #[serde(default, deserialize_with = "null_default")]
    pub authors: Vec<Named>,
    #[serde(default, deserialize_with = "null_default")]
    pub artists: Vec<Named>,
    #[serde(default, deserialize_with = "null_default")]
    pub teams: Vec<Team>,
    #[serde(default, rename = "scanlateStatus")]
    pub scanlate_status: Option<Label>,
    #[serde(default)]
    pub format: Option<Value>,
}

impl TitleDetails {
    /// Описание обычным текстом. Сервер присылает его то строкой, то документом
    /// редактора (`{"type":"doc","content":[...]}`), иногда этот документ ещё и
    /// упакован в строку.
    pub fn summary_text(&self) -> String {
        match &self.summary {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(doc @ Value::Object(_)) => doc_to_text(&doc),
                _ => s.trim().to_string(),
            },
            Some(doc) => doc_to_text(doc),
        }
    }
}

/// Превращает документ редактора (ProseMirror) в текст с абзацами.
pub fn doc_to_text(node: &Value) -> String {
    fn walk(node: &Value, out: &mut String) {
        match node {
            Value::Object(map) => match map.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = map.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
                Some("hardBreak") => out.push('\n'),
                kind => {
                    if let Some(children) = map.get("content") {
                        walk(children, out);
                    }
                    if matches!(kind, Some("paragraph" | "heading" | "listItem")) {
                        out.push('\n');
                    }
                }
            },
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::String(s) => out.push_str(s),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(node, &mut out);
    out.trim().to_string()
}

/// Ограничение на чтение главы: ранний доступ или платная глава.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestrictedView {
    #[serde(default, deserialize_with = "null_default")]
    pub is_open: bool,
    /// Когда глава откроется бесплатно, если это ранний доступ.
    #[serde(default)]
    pub expired_at: Option<String>,
    #[serde(default)]
    pub price: Option<Value>,
}

/// Перевод главы конкретной командой.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterBranch {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub branch_id: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub teams: Vec<Team>,
    #[serde(default)]
    pub user: Option<UserRef>,
    #[serde(default)]
    pub restricted_view: Option<RestrictedView>,
}

impl ChapterBranch {
    /// Глава доступна для чтения этому пользователю.
    pub fn is_open(&self) -> bool {
        self.restricted_view.as_ref().is_none_or(|r| r.is_open)
    }

    /// Подпись переводчика: команда, а если её нет, загрузивший пользователь.
    pub fn translator(&self) -> Option<&str> {
        self.teams
            .first()
            .map(|t| t.name.as_str())
            .or(self.user.as_ref().map(|u| u.username.as_str()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chapter {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub branches_count: u32,
    #[serde(default, deserialize_with = "null_default")]
    pub branches: Vec<ChapterBranch>,
    /// Порядковый номер главы в тайтле, с единицы. Его пишут в закладку.
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub item_number: Option<u64>,
}

impl Chapter {
    pub fn title(&self) -> String {
        let base = format!("Том {} Глава {}", self.volume, self.number);
        match self.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{base} – {name}"),
            _ => base,
        }
    }

    pub fn branch(&self, branch_id: Option<u64>) -> Option<&ChapterBranch> {
        self.branches
            .iter()
            .find(|b| b.branch_id == branch_id)
            .or(self.branches.first())
    }

    pub fn number_f32(&self) -> f32 {
        self.number.parse().unwrap_or(0.0)
    }
}

/// Ветка перевода тайтла целиком (команда, которая ведёт перевод).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub teams: Vec<Team>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageImage {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Порядковый номер страницы, с единицы.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub slug: u64,
    /// Путь от корня сервера картинок, например `//manga/slug/chapters/1/0_x.jpg`.
    pub url: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl PageImage {
    /// Расширение файла по имени картинки.
    pub fn extension(&self) -> &str {
        let name = self.image.as_deref().unwrap_or(&self.url);
        name.rsplit_once('.')
            .map(|(_, ext)| ext)
            .filter(|ext| ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("jpg")
    }
}

/// Содержимое главы манги.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterPages {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub pages: Vec<PageImage>,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub manga_id: Option<u64>,
    /// Глава отмечена прочитанной. Приходит только вошедшему пользователю.
    #[serde(default, deserialize_with = "null_default")]
    pub is_viewed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaServer {
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    pub url: String,
    #[serde(default, deserialize_with = "null_default")]
    pub site_ids: Vec<u8>,
}

impl MediaServer {
    /// Склеивает адрес сервера с путём из ответа. Абсолютный путь не трогает.
    pub fn join(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        format!("{}/{}", self.url.trim_end_matches('/'), path.trim_start_matches('/'))
    }
}

/// Справочник сервера. Нужен прежде всего ради адресов картинок и видео.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Constants {
    #[serde(default, rename = "imageServers", deserialize_with = "null_default")]
    pub image_servers: Vec<MediaServer>,
    #[serde(default, rename = "videoServers", deserialize_with = "null_default")]
    pub video_servers: Vec<MediaServer>,
}

impl Constants {
    /// Серверы картинок для сайта в порядке предпочтения: сначала `preferred`,
    /// потом сжатый (на нём быстрее всего), потом остальные.
    pub fn image_servers_for(&self, site: Site, preferred: &str) -> Vec<&MediaServer> {
        let rank = |s: &MediaServer| match s.id.as_str() {
            id if id == preferred => 0,
            "compress" => 1,
            "main" => 2,
            "secondary" => 3,
            "download" => 4,
            _ => 5,
        };
        let mut servers: Vec<&MediaServer> = self
            .image_servers
            .iter()
            .filter(|s| s.site_ids.contains(&site.id()) && !s.url.is_empty() && s.id != "crop")
            .collect();
        servers.sort_by_key(|s| rank(s));
        let mut seen = std::collections::HashSet::new();
        servers.retain(|s| seen.insert(s.url.clone()));
        servers
    }
}

/// Эпизод в списке.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub season: String,
    #[serde(default)]
    pub item_number: Option<u64>,
    #[serde(default)]
    pub status: Option<Label>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl Episode {
    pub fn title(&self) -> String {
        let base = format!("{} серия", self.number);
        match self.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{base} – {name}"),
            _ => base,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoQuality {
    pub href: String,
    #[serde(deserialize_with = "u64_lenient")]
    pub quality: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub bitrate: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub quality: Vec<VideoQuality>,
}

impl Video {
    /// Качество не выше `max`, иначе самое низкое из доступных.
    pub fn pick(&self, max: u64) -> Option<&VideoQuality> {
        self.quality
            .iter()
            .filter(|q| q.quality <= max)
            .max_by_key(|q| q.quality)
            .or_else(|| self.quality.iter().min_by_key(|q| q.quality))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtitle {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    pub src: String,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerKind {
    /// Собственный хостинг AnimeLib: прямые ссылки на видео.
    Animelib,
    /// Сторонний встраиваемый плеер, открывается в браузере.
    Kodik,
    Other,
}

/// Вариант просмотра эпизода: озвучка или субтитры конкретной команды.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub id: u64,
    pub player: String,
    #[serde(default)]
    pub translation_type: Option<Label>,
    #[serde(default)]
    pub team: Option<Team>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub video: Option<Video>,
    #[serde(default)]
    pub video_domain: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub subtitles: Vec<Subtitle>,
    /// Серия отмечена просмотренной в этом плеере. Только для вошедшего.
    #[serde(default, deserialize_with = "null_default")]
    pub is_viewed: bool,
}

impl Player {
    /// Плеер, команда и тип перевода для закладки. `None`, если чего-то нет.
    pub fn choice(&self) -> Option<crate::bookmarks::PlayerChoice> {
        Some(crate::bookmarks::PlayerChoice {
            player: self.player.clone(),
            team: self.team.as_ref()?.id,
            translation_type: self.translation_type.as_ref()?.id.parse().ok()?,
        })
    }

    pub fn kind(&self) -> PlayerKind {
        match self.player.to_lowercase().as_str() {
            "animelib" => PlayerKind::Animelib,
            "kodik" => PlayerKind::Kodik,
            _ => PlayerKind::Other,
        }
    }

    pub fn team_name(&self) -> &str {
        self.team.as_ref().map(|t| t.name.as_str()).unwrap_or("Без названия")
    }

    /// Адрес встраиваемого плеера со схемой.
    pub fn embed_url(&self) -> Option<String> {
        let src = self.src.as_deref()?;
        Some(if src.starts_with("//") {
            format!("https:{src}")
        } else {
            src.to_string()
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeDetails {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "crate::serde_util::opt_u64_lenient")]
    pub anime_id: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "string_lenient")]
    pub number: String,
    #[serde(default, deserialize_with = "null_default")]
    pub players: Vec<Player>,
}

/// Пользователь, под которым выполнен вход.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
    #[serde(default)]
    pub avatar: Option<Value>,
}

/// Формат картинки по первым байтам: `jpg`, `png`, `webp`, `gif`, `avif`.
/// Расширению в адресе верить нельзя: сервер сжатия отдаёт под именем `.png` JPEG.
pub fn image_format(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        [_, _, _, _, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f' | b's', ..] => Some("avif"),
        _ => None,
    }
}
