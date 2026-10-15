use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::{Branch, Chapter, ChapterPages, MediaServer, PageImage, TitleDetails, image_format};
use crate::site::Site;

/// Поля полной карточки, которые сервер отдаёт только по запросу.
const MANGA_FIELDS: &[&str] = &[
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
    "eng_name",
    "background",
    "views",
    "publisher",
    "franchise",
    "metadata",
    "metadata.count",
    "metadata.close_comments",
    "chap_count",
    "translation_quality_rating",
    "close_view",
];

/// У аниме свой набор: `artists`, `format`, `status_id` сервер для него отвергает.
const ANIME_FIELDS: &[&str] = &[
    "summary",
    "otherNames",
    "genres",
    "tags",
    "authors",
    "teams",
    "rate",
    "releaseDate",
    "anime_status_id",
    "episodes_count",
    "publisher",
    "eng_name",
    "background",
    "views",
    "franchise",
    "metadata",
    "metadata.count",
    "metadata.close_comments",
    "time",
    "shiki_rate",
    "episodesSchedule",
    "close_view",
];

impl Client {
    /// Полная карточка тайтла по `slug_url` (`7965--chainsaw-man`).
    pub async fn title(&self, site: Site, slug_url: &str) -> Result<TitleDetails> {
        let list = if site.is_anime() { ANIME_FIELDS } else { MANGA_FIELDS };
        let fields: Vec<(&str, &str)> = list.iter().map(|f| ("fields[]", *f)).collect();
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
        self.chapter_pages_by(site, slug_url, &chapter.volume, &chapter.number, branch_id)
            .await
    }

    /// То же по тому и номеру главы, когда полной структуры главы под рукой нет.
    pub async fn chapter_pages_by(
        &self,
        site: Site,
        slug_url: &str,
        volume: &str,
        number: &str,
        branch_id: Option<u64>,
    ) -> Result<ChapterPages> {
        let mut query = vec![("volume", volume.to_string()), ("number", number.to_string())];
        if let Some(branch) = branch_id {
            query.push(("branch_id", branch.to_string()));
        }
        let mut pages: ChapterPages = self
            .get_data(site, &format!("manga/{slug_url}/chapter"), &query)
            .await?;
        pages.pages.sort_by_key(|p| p.slug);
        Ok(pages)
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
        let mut urls = self.order_by_health(self.page_urls(site, page, preferred_server).await?);
        let mut last = None;
        let mut limited = None;
        let mut tiny = None;
        // Сначала по одной попытке на каждый сервер, потом ещё круг с повтором,
        // чтобы упавший CDN не множил запросы.
        for retries in [0, 1] {
            let mut i = 0;
            while i < urls.len() {
                let url = urls[i].clone();
                i += 1;
                match self.fetch_bytes_tries(site, &url, retries).await {
                    // Сервер просит паузу: во втором круге к нему не идём, а если
                    // никто не отдаст страницу, вызывающий узнает, сколько ждать.
                    Err(e @ Error::RateLimited { .. }) => {
                        limited = Some(e);
                        i -= 1;
                        urls.remove(i);
                    }
                    // Страница блокировки или капча приходит со статусом 200: не картинка.
                    Ok(bytes) if image_format(&bytes).is_none() => {
                        last = Some(Error::Other(format!("сервер вернул не картинку: {url}")));
                    }
                    // Битые заглушки бывают в сотню байт: пробуем другой сервер,
                    // но если других нет, отдаём что есть.
                    Ok(bytes) if bytes.len() <= 600 => tiny = Some(bytes),
                    Ok(bytes) => return Ok(bytes),
                    Err(e) => last = Some(e),
                }
            }
            if let Some(bytes) = tiny {
                return Ok(bytes);
            }
        }
        Err(limited.or(last).unwrap_or(Error::NotFound))
    }
}
