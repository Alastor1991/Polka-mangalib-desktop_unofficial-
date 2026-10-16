//! Каталог без интерфейса: фильтр, страницы, курсор случайного порядка, повторы
//! между страницами, справочник формы с кэшем.
//!
//! `Catalog` в сеть не ходит. Оболочка берёт у него `Request`, выполняет его на
//! клиенте и отдаёт ответ обратно в `Catalog::apply`. Так экран любой оболочки
//! получает одно и то же поведение: ответ на устаревший запрос (сменили сайт или
//! фильтр, пока грузилось) выбрасывается, второй запрос, пока не вернулся первый,
//! не уходит, а тайтл, который сервер прислал на двух страницах подряд, в списке
//! один раз.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libsocial::catalog::{CatalogFilter, CatalogSort, RandomCursor};
use libsocial::catalog_constants::CatalogConstants;
use libsocial::models::TitleShort;
use libsocial::{BookmarkFolder, Client, Page, Site};

/// Каталог одного сайта с одним фильтром.
#[derive(Debug, Clone)]
pub struct Catalog {
    site: Site,
    filter: CatalogFilter,
    items: Vec<TitleShort>,
    seen: HashSet<u64>,
    /// Следующая страница, `None` – дальше нет.
    next_page: Option<u32>,
    /// Запрос ушёл и ещё не вернулся.
    loading: bool,
    /// Растёт при каждом сбросе списка. Ответ со старым номером выбрасывается.
    generation: u64,
}

/// Метка запроса: по ней `Catalog::apply` узнаёт устаревший ответ. Её удобно
/// носить в сообщении оболочки вместо самого запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
}

/// Запрос страницы каталога. Выполняет его оболочка через `run`.
#[derive(Debug, Clone)]
pub struct Request {
    ticket: Ticket,
    pub site: Site,
    pub filter: CatalogFilter,
    pub page: u32,
}

impl Request {
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }

    pub async fn run(&self, client: &Client) -> libsocial::Result<Page<TitleShort>> {
        client.catalog_filtered(self.site, &self.filter, self.page).await
    }
}

impl Catalog {
    pub fn new(site: Site, filter: CatalogFilter) -> Catalog {
        let mut catalog = Catalog {
            site,
            filter,
            items: Vec::new(),
            seen: HashSet::new(),
            next_page: Some(1),
            loading: false,
            generation: 0,
        };
        catalog.fit_filter();
        catalog
    }

    pub fn site(&self) -> Site {
        self.site
    }

    pub fn filter(&self) -> &CatalogFilter {
        &self.filter
    }

    pub fn items(&self) -> &[TitleShort] {
        &self.items
    }

    pub fn has_next(&self) -> bool {
        self.next_page.is_some()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Сортировки, которые можно выбрать на этом сайте.
    pub fn sorts(&self) -> &'static [CatalogSort] {
        CatalogSort::available(self.filter.is_anime(self.site))
    }

    /// Другой сайт с тем же фильтром. Сортировка и поля карточки, которых у
    /// нового сайта нет, сбрасываются: сервер ответил бы на них 422.
    pub fn set_site(&mut self, site: Site) {
        self.site = site;
        self.fit_filter();
        self.reset();
    }

    /// Новый фильтр. Курсор случайного порядка от старого не подходит.
    pub fn set_filter(&mut self, filter: CatalogFilter) {
        self.filter = filter;
        self.fit_filter();
        self.reset();
    }

    /// Тот же фильтр с первой страницы. При случайном порядке это новая выборка.
    pub fn reload(&mut self) {
        self.reset();
    }

    /// Следующая страница, если она есть и предыдущий запрос уже вернулся.
    /// После ошибки просто зовите снова: страница та же.
    pub fn next_request(&mut self) -> Option<Request> {
        if self.loading {
            return None;
        }
        let page = self.next_page?;
        self.loading = true;
        let mut filter = self.filter.clone();
        // Сайт сам всегда называет себя в `site_id[]`. Без него сервер берёт сайт
        // из `Site-Id`, но из MangaLib тогда видно и ранобэ.
        if filter.site_ids.is_empty() {
            filter.site_ids = vec![self.site.id()];
        }
        Some(Request {
            ticket: Ticket {
                generation: self.generation,
            },
            site: self.site,
            filter,
            page,
        })
    }

    /// Ответ на запрос. Отдаёт тайтлы, которые добавились в конец списка: оболочке
    /// по ним грузить обложки. Ответ на устаревший запрос не меняет ничего.
    pub fn apply<E>(&mut self, ticket: Ticket, result: Result<Page<TitleShort>, E>) -> Result<&[TitleShort], E> {
        if ticket.generation != self.generation {
            return Ok(&[]);
        }
        self.loading = false;
        let page = result?;
        let mut has_next = page.has_next;
        if self.filter.sort == CatalogSort::Random {
            self.filter.random = RandomCursor::of(&page);
            // Без курсора вторую страницу случайного порядка не получить.
            has_next &= self.filter.random.is_some();
        }
        self.next_page = match self.next_page {
            Some(current) if has_next => Some(current + 1),
            _ => None,
        };
        let start = self.items.len();
        for item in page.items {
            if self.seen.insert(item.id) {
                self.items.push(item);
            }
        }
        Ok(&self.items[start..])
    }

    fn reset(&mut self) {
        self.generation += 1;
        self.items.clear();
        self.seen.clear();
        self.next_page = Some(1);
        self.loading = false;
        self.filter.random = None;
    }

    fn fit_filter(&mut self) {
        let anime = self.filter.is_anime(self.site);
        if !self.filter.sort.supports(anime) {
            self.filter.sort = CatalogSort::default();
        }
        self.filter.fields.retain(|f| f.supports(anime));
    }
}

/// Папка закладок, в которой лежит тайтл, для пометки на карточке.
pub fn bookmark_folder<'a>(title: &TitleShort, folders: &'a [BookmarkFolder]) -> Option<&'a BookmarkFolder> {
    let status = title.user_bookmark.as_ref()?.status;
    folders.iter().find(|f| f.id == status)
}

/// Справочник формы каталога (жанры, теги, типы, статусы) с кэшем в памяти.
///
/// Ответ тяжёлый, а меняется редко: сайт держит его 20 минут, столько же и здесь.
/// Пока справочник грузится, остальные просящие ждут тот же ответ, а не шлют свой.
/// Если обновить не вышло, отдаётся прежний, и следующая попытка будет через
/// тот же срок.
#[derive(Clone)]
pub struct Constants {
    slot: Arc<tokio::sync::Mutex<Option<Fetched>>>,
    ttl: Duration,
}

/// Справочник и когда его получили.
type Fetched = (Instant, Arc<CatalogConstants>);

impl Constants {
    pub const TTL: Duration = Duration::from_secs(20 * 60);

    pub fn new() -> Constants {
        Constants::with_ttl(Constants::TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Constants {
        Constants {
            slot: Arc::new(tokio::sync::Mutex::new(None)),
            ttl,
        }
    }

    pub async fn get(&self, client: &Client) -> libsocial::Result<Arc<CatalogConstants>> {
        let mut slot = self.slot.lock().await;
        if let Some((at, constants)) = slot.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(constants.clone());
        }
        match client.catalog_constants().await {
            Ok(fresh) => {
                let fresh = Arc::new(fresh);
                *slot = Some((Instant::now(), fresh.clone()));
                Ok(fresh)
            }
            // Прежний справочник служит ещё один срок: иначе, пока сервер лежит,
            // каждый просящий снова шёл бы в сеть и ждал отказа.
            Err(e) => match slot.take() {
                Some((_, old)) => {
                    *slot = Some((Instant::now(), old.clone()));
                    Ok(old)
                }
                None => Err(e),
            },
        }
    }
}

impl Default for Constants {
    fn default() -> Self {
        Constants::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsocial::catalog::CatalogField;
    use serde_json::{Value, json};

    fn title(id: u64) -> TitleShort {
        serde_json::from_value(json!({"id": id, "slug_url": format!("{id}--t")})).unwrap()
    }

    fn page(ids: &[u64], has_next: bool, meta: Value) -> Page<TitleShort> {
        Page {
            items: ids.iter().map(|id| title(*id)).collect(),
            has_next,
            meta,
            ..Page::empty()
        }
    }

    fn ids(c: &Catalog) -> Vec<u64> {
        c.items().iter().map(|t| t.id).collect()
    }

    #[test]
    fn pages_follow_and_repeats_drop() {
        let mut c = Catalog::new(Site::MangaLib, CatalogFilter::default());
        let first = c.next_request().unwrap();
        assert_eq!((first.page, first.filter.site_ids.as_slice()), (1, &[1u8][..]));
        // Пока первый не вернулся, второй не уходит.
        assert!(c.next_request().is_none() && c.is_loading());

        let added: Vec<u64> = c
            .apply::<()>(first.ticket(), Ok(page(&[1, 2, 3], true, Value::Null)))
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(added, vec![1, 2, 3]);

        let second = c.next_request().unwrap();
        assert_eq!(second.page, 2);
        let added = c
            .apply::<()>(second.ticket(), Ok(page(&[3, 4], false, Value::Null)))
            .unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(ids(&c), vec![1, 2, 3, 4]);
        assert!(!c.has_next() && c.next_request().is_none());
    }

    #[test]
    fn error_keeps_page_and_stale_answer_is_dropped() {
        let mut c = Catalog::new(Site::MangaLib, CatalogFilter::default());
        let first = c.next_request().unwrap();
        assert!(matches!(c.apply(first.ticket(), Err("сеть")), Err("сеть")));
        let again = c.next_request().unwrap();
        assert_eq!(again.page, 1);

        // Пока грузилось, сменили сайт: старый ответ в новый список не попадает.
        c.set_site(Site::AnimeLib);
        assert!(
            c.apply::<()>(again.ticket(), Ok(page(&[9], true, Value::Null)))
                .unwrap()
                .is_empty()
        );
        assert!(c.items().is_empty() && !c.is_loading());
        let fresh = c.next_request().unwrap();
        assert_eq!((fresh.site, fresh.page), (Site::AnimeLib, 1));
    }

    #[test]
    fn random_order_carries_cursor() {
        let filter = CatalogFilter {
            sort: CatalogSort::Random,
            ..Default::default()
        };
        let mut c = Catalog::new(Site::MangaLib, filter);
        let first = c.next_request().unwrap();
        assert!(first.filter.random.is_none());
        let meta = json!({"seed": "abc", "random_order": 0.5});
        c.apply::<()>(first.ticket(), Ok(page(&[1], true, meta))).unwrap();
        let second = c.next_request().unwrap();
        assert_eq!(second.page, 2);
        assert_eq!(second.filter.random.as_ref().map(|r| r.seed.as_str()), Some("abc"));

        // Сервер не прислал курсор: дальше не листаем, а не ловим 422.
        c.apply::<()>(second.ticket(), Ok(page(&[2], true, Value::Null)))
            .unwrap();
        assert!(!c.has_next());

        c.reload();
        assert!(c.filter().random.is_none() && c.has_next() && c.items().is_empty());
    }

    #[test]
    fn site_change_drops_what_server_would_reject() {
        let filter = CatalogFilter {
            sort: CatalogSort::ChapterCount,
            fields: vec![CatalogField::Rate, CatalogField::MangaStatusId],
            site_ids: Vec::new(),
            ..Default::default()
        };
        let mut c = Catalog::new(Site::MangaLib, filter);
        assert_eq!(c.filter().sort, CatalogSort::ChapterCount);
        c.set_site(Site::AnimeLib);
        assert_eq!(c.filter().sort, CatalogSort::Popularity);
        assert_eq!(c.filter().fields, vec![CatalogField::Rate]);
        assert!(c.sorts().contains(&CatalogSort::EpisodeCount));
        // Заданные сайты фильтра не подменяются.
        let mut own = Catalog::new(
            Site::MangaLib,
            CatalogFilter {
                site_ids: vec![1, 3],
                ..Default::default()
            },
        );
        assert_eq!(own.next_request().unwrap().filter.site_ids, vec![1, 3]);
    }

    #[test]
    fn folder_mark() {
        let folders: Vec<BookmarkFolder> =
            serde_json::from_value(json!([{"id": 1, "name": "Читаю"}, {"id": 2, "name": "В планах"}])).unwrap();
        let mut t = title(1);
        assert!(bookmark_folder(&t, &folders).is_none());
        t.user_bookmark = serde_json::from_value(json!({"id": 5, "status": 2})).unwrap();
        assert_eq!(bookmark_folder(&t, &folders).map(|f| f.name.as_str()), Some("В планах"));
    }

    mod constants {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn server(times: u64) -> (MockServer, Client) {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/constants"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": {"genres": [{"id": 34, "name": "Боевик", "site_ids": [1]}]}})),
                )
                .expect(times)
                .mount(&server)
                .await;
            let client = Client::new(libsocial::ClientConfig {
                api_mirrors: vec![format!("{}/api", server.uri())],
                api_interval: Duration::ZERO,
                ..Default::default()
            })
            .unwrap();
            (server, client)
        }

        #[tokio::test]
        async fn one_request_while_fresh() {
            let (server, client) = server(1).await;
            let cache = Constants::new();
            let (a, b) = tokio::join!(cache.get(&client), cache.get(&client));
            assert_eq!(a.unwrap().genres_for(1).len(), 1);
            assert!(Arc::ptr_eq(&b.unwrap(), &cache.get(&client).await.unwrap()));
            server.verify().await;
        }

        #[tokio::test]
        async fn stale_is_fetched_again_and_kept_on_failure() {
            let (server, client) = server(2).await;
            let cache = Constants::with_ttl(Duration::ZERO);
            cache.get(&client).await.unwrap();
            cache.get(&client).await.unwrap();
            server.verify().await;

            // Сервер пропал: отдаём то, что было. Брошенный MockServer wiremock
            // возвращает в общий пул, и его адрес может достаться соседнему тесту,
            // поэтому недоступный сервер изображает клиент на закрытом порту.
            let offline = Client::new(libsocial::ClientConfig {
                api_mirrors: vec!["http://127.0.0.1:9/api".into()],
                api_interval: Duration::ZERO,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(cache.get(&offline).await.unwrap().genres_for(1).len(), 1);
        }

        #[tokio::test]
        async fn failed_refresh_waits_a_full_term() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/constants"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"genres": []}})))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/constants"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount(&server)
                .await;
            let client = Client::new(libsocial::ClientConfig {
                api_mirrors: vec![format!("{}/api", server.uri())],
                api_interval: Duration::ZERO,
                ..Default::default()
            })
            .unwrap();
            let cache = Constants::with_ttl(Duration::from_millis(200));
            cache.get(&client).await.unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
            // Обновить не вышло: старое отдаётся, а следующий просящий в сеть не идёт.
            cache.get(&client).await.unwrap();
            cache.get(&client).await.unwrap();
            server.verify().await;
        }
    }
}
