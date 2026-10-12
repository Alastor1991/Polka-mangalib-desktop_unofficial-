use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::{
    Branch, Chapter, ChapterPages, Constants, MediaServer, PageImage, Paged, TitleDetails, TitleShort,
};
use crate::site::Site;

/// Поля полной карточки, которые сервер отдаёт только по запросу.
const DETAIL_FIELDS: &[&str] = &[
    "summary",
    "otherNames",
    "genres",
    "tags",
    "authors",
    "artists",
    "teams",
    "format",
    "rate",
    "releaseDate",
    "status_id",
    "manga_status_id",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Порядок сервера по умолчанию (популярность).
    #[default]
    Popular,
    Rating,
    Views,
    LastUpdate,
    Newest,
    Name,
}

impl Sort {
    fn param(self) -> Option<&'static str> {
        match self {
            Sort::Popular => None,
            Sort::Rating => Some("rate_avg"),
            Sort::Views => Some("views"),
            Sort::LastUpdate => Some("last_chapter_at"),
            Sort::Newest => Some("created_at"),
            Sort::Name => Some("rus_name"),
        }
    }
}

/// Запрос к каталогу. Пустой запрос даёт популярное.
#[derive(Debug, Clone, Default)]
pub struct CatalogQuery {
    pub text: String,
    pub page: u32,
    pub sort: Sort,
    pub ascending: bool,
    pub genres: Vec<u64>,
    pub genres_exclude: Vec<u64>,
    /// Только тайтлы, где есть хотя бы одна глава или эпизод.
    pub with_content: bool,
}

impl CatalogQuery {
    pub fn search(text: impl Into<String>) -> CatalogQuery {
        CatalogQuery {
            text: text.into(),
            page: 1,
            ..Default::default()
        }
    }

    fn params(&self, site: Site) -> Vec<(String, String)> {
        let mut p = vec![
            ("site_id[]".to_string(), site.id().to_string()),
            ("page".into(), self.page.max(1).to_string()),
        ];
        for field in ["rate", "rate_avg", "userBookmark"] {
            p.push(("fields[]".into(), field.into()));
        }
        if !self.text.trim().is_empty() {
            p.push(("q".into(), self.text.trim().to_string()));
        }
        if let Some(sort) = self.sort.param() {
            p.push(("sort_by".into(), sort.into()));
        }
        p.push(("sort_type".into(), if self.ascending { "asc" } else { "desc" }.into()));
        p.extend(self.genres.iter().map(|g| ("genres[]".to_string(), g.to_string())));
        p.extend(
            self.genres_exclude
                .iter()
                .map(|g| ("genres_exclude[]".to_string(), g.to_string())),
        );
        if self.with_content {
            let key = if site.is_anime() {
                "episodes_count_min"
            } else {
                "chap_count_min"
            };
            p.push((key.into(), "1".into()));
        }
        p
    }
}

impl Client {
    /// Каталог или поиск по сайту. Для AnimeLib идёт в раздел аниме.
    pub async fn catalog(&self, site: Site, query: &CatalogQuery) -> Result<Paged<TitleShort>> {
        self.get_json(site, site.catalog_path(), &query.params(site)).await
    }

    pub async fn search(&self, site: Site, text: &str, page: u32) -> Result<Paged<TitleShort>> {
        let query = CatalogQuery {
            text: text.into(),
            page,
            ..Default::default()
        };
        self.catalog(site, &query).await
    }

    /// Полная карточка тайтла по `slug_url` (`7965--chainsaw-man`).
    pub async fn title(&self, site: Site, slug_url: &str) -> Result<TitleDetails> {
        let fields: Vec<(&str, &str)> = DETAIL_FIELDS.iter().map(|f| ("fields[]", *f)).collect();
        self.get_data(site, &format!("{}/{}", site.catalog_path(), slug_url), &fields)
            .await
    }

    /// Все главы тайтла в порядке сервера (от первой к последней).
    pub async fn chapters(&self, site: Site, slug_url: &str) -> Result<Vec<Chapter>> {
        self.get_data(site, &format!("manga/{slug_url}/chapters"), &()).await
    }

    /// Ветки перевода тайтла. Первая в списке – ветка по умолчанию на сайте.
    pub async fn branches(&self, site: Site, title_id: u64) -> Result<Vec<Branch>> {
        self.get_data(site, &format!("branches/{title_id}"), &()).await
    }

    /// Страницы главы. Для закрытой главы сервер отвечает ошибкой, сюда её лучше не слать.
    pub async fn chapter_pages(
        &self,
        site: Site,
        slug_url: &str,
        chapter: &Chapter,
        branch_id: Option<u64>,
    ) -> Result<ChapterPages> {
        let mut query = vec![("volume", chapter.volume.clone()), ("number", chapter.number.clone())];
        if let Some(branch) = branch_id {
            query.push(("branch_id", branch.to_string()));
        }
        let mut pages: ChapterPages = self
            .get_data(site, &format!("manga/{slug_url}/chapter"), &query)
            .await?;
        pages.pages.sort_by_key(|p| p.slug);
        Ok(pages)
    }

    /// Справочник сервера, кэшируется на время жизни клиента.
    pub async fn constants(&self) -> Result<Constants> {
        if let Some(c) = self.constants.read().await.clone() {
            return Ok(c);
        }
        let query = [("fields[]", "imageServers"), ("fields[]", "videoServers")];
        let constants: Constants = self.get_data(Site::MangaLib, "constants", &query).await?;
        *self.constants.write().await = Some(constants.clone());
        Ok(constants)
    }

    /// Адреса страницы на всех подходящих серверах, в порядке предпочтения.
    pub async fn page_urls(&self, site: Site, page: &PageImage, preferred_server: &str) -> Result<Vec<String>> {
        let constants = self.constants().await?;
        let servers: Vec<&MediaServer> = constants.image_servers_for(site, preferred_server);
        if servers.is_empty() {
            return Err(Error::Other(format!(
                "сервер не прислал адресов картинок для {}",
                site.name()
            )));
        }
        Ok(servers.iter().map(|s| s.join(&page.url)).collect())
    }

    /// Скачивает страницу, перебирая серверы, пока один не ответит.
    pub async fn fetch_page(&self, site: Site, page: &PageImage, preferred_server: &str) -> Result<bytes::Bytes> {
        let mut last = None;
        for url in self.page_urls(site, page, preferred_server).await? {
            match self.fetch_bytes(site, &url).await {
                // Битые заглушки сервера бывают в сотню байт.
                Ok(bytes) if bytes.len() > 600 => return Ok(bytes),
                Ok(_) => last = Some(Error::Other(format!("сервер вернул пустую картинку: {url}"))),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(Error::NotFound))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_params() {
        let q = CatalogQuery {
            text: " берсерк ".into(),
            page: 2,
            sort: Sort::Rating,
            with_content: true,
            ..Default::default()
        };
        let p = q.params(Site::MangaLib);
        assert!(p.contains(&("site_id[]".into(), "1".into())));
        assert!(p.contains(&("q".into(), "берсерк".into())));
        assert!(p.contains(&("sort_by".into(), "rate_avg".into())));
        assert!(p.contains(&("chap_count_min".into(), "1".into())));
        let p = CatalogQuery::default().params(Site::AnimeLib);
        assert!(p.contains(&("site_id[]".into(), "5".into())));
        assert!(p.contains(&("page".into(), "1".into())));
    }
}
