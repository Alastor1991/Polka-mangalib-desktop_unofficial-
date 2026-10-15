//! Всё вокруг тайтла, кроме глав и закладок: обложки, статистика, связанные и
//! похожие тайтлы, персонажи, страницы людей, франшиз и издателей, подписки,
//! оценка тайтла, лайк главы и оценка перевода, подписки на ветки и озвучки,
//! команды переводчиков, отзывы, озвучка манги.
//!
//! Путь у манги, ранобэ, SlashLib и HentaiLib один (`manga`), у аниме свой. Вместо
//! `slug_url` сервер везде, кроме карточки команды, принимает и числовой id.

use std::collections::HashMap;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::comments::{Vote, Votes};
use crate::error::Result;
use crate::listing::Page;
use crate::models::{Cover, Label, Rating, TitleShort, TranslationRating, Video};
use crate::rich::Doc;
use crate::search::{EntityCard, EntityKind};
use crate::serde_util::{null_default, opt_string_lenient, opt_u64_lenient, u64_lenient};
use crate::site::Site;

/// Обложка из галереи тайтла.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverImage {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub cover: Option<FullCover>,
    /// Подпись: том или текст.
    #[serde(default, deserialize_with = "opt_string_lenient")]
    pub info: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FullCover {
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub md: Option<String>,
    /// Оригинал, для просмотрщика.
    #[serde(default)]
    pub orig: Option<String>,
}

/// Строка статистики: папка закладок или оценка.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatRow {
    /// Название папки или оценка числом.
    #[serde(default, deserialize_with = "crate::serde_util::string_lenient")]
    pub label: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
    #[serde(default)]
    pub percent: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatBlock {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub count: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub stats: Vec<StatRow>,
}

/// Распределение по папкам закладок и по оценкам.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleStats {
    #[serde(default)]
    pub bookmarks: StatBlock,
    #[serde(default)]
    pub rating: StatBlock,
}

/// Связанный тайтл. Бывает на другом сайте сети: аниме по манге и наоборот.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub order: u64,
    /// Продолжение, адаптация, спин-офф...
    #[serde(default)]
    pub related_type: Option<Label>,
    pub media: TitleShort,
}

/// Похожий тайтл с голосами пользователей.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Similar {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    /// Чем похож: «по жанрам», «по рисовке», «по сюжету».
    #[serde(default, deserialize_with = "null_default")]
    pub similar: String,
    pub media: TitleShort,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: Votes,
}

/// Чем похожи тайтлы, когда пользователь добавляет пару.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarBy {
    Genres = 1,
    Art = 2,
    Plot = 3,
}

/// Роль персонажа в тайтле.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharacterRole {
    Main,
    Supporting,
    Background,
}

impl CharacterRole {
    fn param(self) -> &'static str {
        match self {
            CharacterRole::Main => "Main",
            CharacterRole::Supporting => "Supporting",
            CharacterRole::Background => "Background",
        }
    }
}

/// Подписка-звёздочка на человека, персонажа, франшизу, издателя, команду.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Favorite {
    #[serde(default, deserialize_with = "null_default")]
    pub is_subscribed: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub source_type: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub source_id: u64,
}

/// Число с подписью из карточек: «Тайтлов», «Подписчиков».
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityStat {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
    #[serde(default)]
    pub short: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    /// `titles`, `subscribes`, `likes`, `chapters`, `chapters_per_month`.
    #[serde(default, deserialize_with = "null_default")]
    pub tag: String,
}

/// Персонаж тайтла: карточка и его роль в этом тайтле.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleCharacter {
    #[serde(flatten)]
    pub card: EntityCard,
    /// Роль и порядок: `{"order": 0, "position": {"id": "Main", "label": "Главный"}}`.
    #[serde(default)]
    pub details: Option<Value>,
}

/// Карточка команды переводчиков.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamCard {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default)]
    pub slug_url: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub alt_name: Option<String>,
    #[serde(default)]
    pub cover: Option<Cover>,
    #[serde(default)]
    pub background: Option<crate::models::Background>,
    #[serde(default)]
    pub vk: Option<String>,
    #[serde(default)]
    pub discord: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub description: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub stats: Vec<EntityStat>,
    #[serde(default, deserialize_with = "null_default")]
    pub titles_count_details: HashMap<String, u64>,
    #[serde(default)]
    pub team_rating: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub ignored_by_user: bool,
}

impl TeamCard {
    pub fn description(&self) -> Doc {
        self.description.as_ref().map(Doc::from_prosemirror).unwrap_or_default()
    }
}

/// Участник команды.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    #[serde(default)]
    pub user: Option<crate::comments::Author>,
    #[serde(default, deserialize_with = "null_default")]
    pub roles_string: String,
}

/// Порядок списка команд.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TeamSort {
    #[default]
    Name,
    Titles,
    Subscribers,
    Likes,
}

impl TeamSort {
    fn param(self) -> &'static str {
        match self {
            TeamSort::Name => "name",
            TeamSort::Titles => "titles_count",
            TeamSort::Subscribers => "subscribes_count",
            TeamSort::Likes => "likes_count",
        }
    }
}

/// Оценка команды по пяти критериям, каждый 1–10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TeamScore {
    pub translation: u8,
    pub literacy: u8,
    pub adaptation: u8,
    pub formatting: u8,
    pub stability: u8,
}

/// Оценка перевода главы по трём критериям, каждый 1–10. Все нули снимают оценку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TranslationScore {
    pub translation_accuracy: u8,
    pub readability_adaptation: u8,
    pub editing_formatting: u8,
}

/// Отзыв или рецензия.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub title: String,
    /// Текст документом ProseMirror. Разобранный – `Review::text`.
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub views: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub comments_count: u64,
    #[serde(default)]
    pub user: Option<crate::comments::Author>,
    /// Критерии с оценками 1–10, у мини-отзыва один «Итоговая оценка».
    #[serde(default, deserialize_with = "null_default")]
    pub rating: Vec<ReviewScore>,
    /// `short` – отзыв, `full` – рецензия.
    #[serde(default, rename = "type")]
    pub kind: Option<Label>,
    /// `positive`, `neutral`, `negative`.
    #[serde(default)]
    pub evaluation: Option<Label>,
    /// `published` или `draft`.
    #[serde(default)]
    pub status: Option<Label>,
    #[serde(default, deserialize_with = "null_default")]
    pub votes: Votes,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Тайтл отзыва. Есть в одном отзыве, в списке нет.
    #[serde(default)]
    pub related: Option<TitleShort>,
}

impl Review {
    pub fn text(&self) -> Doc {
        self.content.as_ref().map(Doc::from_prosemirror).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewScore {
    #[serde(default, deserialize_with = "null_default")]
    pub label: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub value: u64,
}

/// Какие отзывы показывать.
#[derive(Debug, Clone, Default)]
pub struct ReviewFilter {
    /// Отзывы тайтла: `(site, id тайтла)`.
    pub title: Option<(Site, u64)>,
    pub user_id: Option<u64>,
    /// `short` или `full`, `None` – все.
    pub kind: Option<String>,
    /// `positive`, `neutral`, `negative`, `None` – все.
    pub evaluation: Option<String>,
    /// `newest`, `oldest`, `votes`, `views`.
    pub sort: Option<String>,
    pub page: u32,
}

/// Новый отзыв или правка.
#[derive(Debug, Clone)]
pub struct ReviewDraft {
    pub site: Site,
    pub title_id: u64,
    pub title: String,
    pub text: Doc,
    /// `positive`, `neutral`, `negative`.
    pub evaluation: String,
    /// Итоговая оценка 1–10.
    pub score: u8,
    /// Рецензия, а не короткий отзыв.
    pub full: bool,
    /// Черновик, не публиковать.
    pub draft: bool,
}

/// Озвучка манги: видео, где читают вслух несколько глав.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MangaVoice {
    #[serde(deserialize_with = "u64_lenient")]
    pub id: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub manga_id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub player: String,
    #[serde(default)]
    pub language: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub teams: Vec<crate::models::Team>,
    #[serde(default)]
    pub video: Option<Video>,
    /// Какие главы читают: `{volume, chapter}`.
    #[serde(default, deserialize_with = "null_default")]
    pub chapters: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub is_viewed: bool,
}

/// Подписка на озвучку или субтитры: команда (`0` – любая) и тип перевода.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSubscription {
    #[serde(default, deserialize_with = "u64_lenient")]
    pub team_id: u64,
    /// У аниме: 1 субтитры, 2 озвучка. У манги нет.
    #[serde(
        default,
        deserialize_with = "opt_u64_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub translation_type: Option<u64>,
}

impl Client {
    pub async fn covers(&self, site: Site, slug_url: &str) -> Result<Vec<CoverImage>> {
        self.get_data(site, &format!("{}/{slug_url}/covers", site.catalog_path()), &())
            .await
    }

    /// Распределение по папкам закладок и по оценкам.
    pub async fn title_stats(&self, site: Site, slug_url: &str) -> Result<TitleStats> {
        // Без обоих параметров сервер присылает пустой ответ.
        let query = [("bookmarks", "true"), ("rating", "true")];
        self.get_data(site, &format!("{}/{slug_url}/stats", site.catalog_path()), &query)
            .await
    }

    pub async fn relations(&self, site: Site, slug_url: &str) -> Result<Vec<Relation>> {
        let mut list: Vec<Relation> = self
            .get_data(site, &format!("{}/{slug_url}/relations", site.catalog_path()), &())
            .await?;
        list.sort_by_key(|r| r.order);
        Ok(list)
    }

    /// Похожие тайтлы, лучшие по голосам сверху. Бывают сотни.
    pub async fn similar(&self, site: Site, slug_url: &str) -> Result<Vec<Similar>> {
        let mut list: Vec<Similar> = self
            .get_data(site, &format!("{}/{slug_url}/similar", site.catalog_path()), &())
            .await?;
        list.sort_by_key(|s| std::cmp::Reverse(s.votes.score()));
        Ok(list)
    }

    pub async fn vote_similar(&self, id: u64, vote: Vote) -> Result<()> {
        self.vote("similar", id, vote).await
    }

    /// Предлагает пару похожих тайтлов, один или два критерия.
    pub async fn add_similar(
        &self,
        site: Site,
        title_id: u64,
        similar: &TitleShort,
        by: &[SimilarBy],
    ) -> Result<Similar> {
        let similar_site = similar.site().unwrap_or(site);
        let body = json!({
            "source_type": site.catalog_path(),
            "source_id": title_id,
            "similar_id": similar.id,
            "similar_type": similar_site.catalog_path(),
            "similar_by": by.iter().map(|b| *b as u8).collect::<Vec<_>>(),
        });
        self.send_data(site, Method::POST, "similar", &(), Some(&body)).await
    }

    /// Персонажи тайтла. `role: None` – все роли сразу.
    pub async fn title_characters(
        &self,
        site: Site,
        title_id: u64,
        role: Option<CharacterRole>,
        page: u32,
    ) -> Result<Page<TitleCharacter>> {
        let mut query = vec![
            ("media_id", title_id.to_string()),
            ("media_type", site.catalog_path().to_string()),
            ("page", page.max(1).to_string()),
            ("limit", "30".to_string()),
        ];
        if let Some(role) = role {
            query.push(("position", role.param().to_string()));
        }
        self.get_page(site, "character", &query).await
    }

    /// Состояние подписки-звёздочки.
    pub async fn favorite(&self, kind: EntityKind, id: u64) -> Result<Favorite> {
        self.get_data(Site::MangaLib, &format!("favorites/{}/{id}", kind.model()), &())
            .await
    }

    /// Переключает подписку-звёздочку. Возвращает новое состояние.
    pub async fn toggle_favorite(&self, kind: EntityKind, id: u64) -> Result<Favorite> {
        let body = json!({"source_type": kind.model(), "source_id": id});
        self.send_data(Site::MangaLib, Method::POST, "favorites", &(), Some(&body))
            .await
    }

    /// Своя оценка тайтла 1–10, `0` снимает оценку. Возвращает новый рейтинг.
    pub async fn rate_title(&self, site: Site, title_id: u64, score: u8) -> Result<Rating> {
        let body = json!({"score": score.min(10), "rateable_id": title_id, "rateable_type": site.catalog_path()});
        self.send_data(
            site,
            Method::POST,
            &format!("{}/rate", site.catalog_path()),
            &(),
            Some(&body),
        )
        .await
    }

    /// Переключает лайк главы («Сказать спасибо»). `chapter_id` – id перевода в ветке.
    pub async fn like_chapter(&self, chapter_id: u64) -> Result<()> {
        self.send_ok::<()>(
            Site::MangaLib,
            Method::POST,
            &format!("chapters/{chapter_id}/like"),
            None,
        )
        .await
    }

    /// Оценка перевода главы. Возвращает новую оценку главы.
    pub async fn rate_translation(&self, chapter_id: u64, score: TranslationScore) -> Result<TranslationRating> {
        self.send_data(
            Site::MangaLib,
            Method::POST,
            &format!("chapters/{chapter_id}/translation-rating"),
            &(),
            Some(&score),
        )
        .await
    }

    /// Переключает подписку на новые главы ветки (`Branch::id`). Возвращает ветку с
    /// новым `notify`.
    pub async fn toggle_branch_subscription(&self, site: Site, branch_id: u64) -> Result<crate::models::Branch> {
        self.send_data::<_, (), ()>(
            site,
            Method::POST,
            &format!("branches/{branch_id}/subscribe"),
            &(),
            None,
        )
        .await
    }

    /// Подписки на озвучку (манга) или озвучку и субтитры (аниме).
    pub async fn voice_subscriptions(&self, site: Site, slug_url: &str) -> Result<Vec<VoiceSubscription>> {
        self.get_data(site, &format!("{}/{slug_url}/subscriptions", site.catalog_path()), &())
            .await
    }

    /// Заменяет подписки на озвучку. Пустой список снимает все.
    pub async fn set_voice_subscriptions(
        &self,
        site: Site,
        title_id: u64,
        items: &[VoiceSubscription],
    ) -> Result<Vec<VoiceSubscription>> {
        let body = json!({"items": items});
        self.send_data(
            site,
            Method::POST,
            &format!("{}/{title_id}/subscriptions", site.catalog_path()),
            &(),
            Some(&body),
        )
        .await
    }

    // --- Команды ---

    /// Карточка команды. Принимает только `slug_url`, по числу сервер отвечает 404.
    /// Открывается только вошедшему: гостю тоже 404.
    pub async fn team(&self, slug_url: &str) -> Result<TeamCard> {
        let query = [
            ("fields[]", "chaptersPerMonth"),
            ("fields[]", "team_rating"),
            ("fields[]", "ignored_by_user"),
        ];
        self.get_data(Site::MangaLib, &format!("teams/{slug_url}"), &query)
            .await
    }

    pub async fn teams(&self, text: &str, sort: TeamSort, page: u32) -> Result<Page<TeamCard>> {
        let mut query = vec![
            ("page", page.max(1).to_string()),
            ("sort_by", sort.param().to_string()),
            (
                "sort_type",
                if sort == TeamSort::Name { "asc" } else { "desc" }.to_string(),
            ),
        ];
        if text.trim().chars().count() >= 2 {
            query.push(("q", text.trim().to_string()));
        }
        self.get_page(Site::MangaLib, "teams", &query).await
    }

    /// Участники команды. Здесь, наоборот, нужен числовой id.
    pub async fn team_members(&self, team_id: u64) -> Result<Vec<TeamMember>> {
        self.get_data(Site::MangaLib, &format!("teams/{team_id}/users"), &())
            .await
    }

    /// Свежие главы команды, сгруппированные по тайтлам, по 16 групп.
    pub async fn team_chapters(&self, team_id: u64, page: u32) -> Result<Page<Value>> {
        self.get_page(
            Site::MangaLib,
            &format!("teams/{team_id}/chapters"),
            &[("page", page.max(1))],
        )
        .await
    }

    pub async fn rate_team(&self, team_id: u64, score: TeamScore) -> Result<()> {
        self.send_ok(
            Site::MangaLib,
            Method::POST,
            &format!("teams/{team_id}/rating"),
            Some(&score),
        )
        .await
    }

    pub async fn unrate_team(&self, team_id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("teams/{team_id}/rating"), None)
            .await
    }

    /// Скрыть переводы команды (`true`) или вернуть их.
    pub async fn ignore_team(&self, team_id: u64, ignore: bool) -> Result<()> {
        let method = if ignore { Method::POST } else { Method::DELETE };
        self.send_ok::<()>(Site::MangaLib, method, &format!("teams/{team_id}/ignore"), None)
            .await
    }

    // --- Отзывы ---

    pub async fn reviews(&self, filter: &ReviewFilter) -> Result<Page<Review>> {
        let mut query = vec![("page", filter.page.max(1).to_string())];
        if let Some((site, id)) = filter.title {
            query.push(("reviewable_type", site.catalog_path().to_string()));
            query.push(("reviewable_id", id.to_string()));
        }
        if let Some(user) = filter.user_id {
            query.push(("user_id", user.to_string()));
        }
        for (key, value) in [
            ("type", &filter.kind),
            ("evaluation", &filter.evaluation),
            ("sort_by", &filter.sort),
        ] {
            if let Some(v) = value.as_ref().filter(|v| v.as_str() != "all") {
                query.push((key, v.clone()));
            }
        }
        let site = filter.title.map(|(s, _)| s).unwrap_or(Site::MangaLib);
        self.get_page(site, "reviews", &query).await
    }

    pub async fn review(&self, id: u64) -> Result<Review> {
        self.get_data(Site::MangaLib, &format!("reviews/{id}"), &()).await
    }

    fn review_body(d: &ReviewDraft) -> Value {
        json!({
            "title": d.title,
            "evaluation": d.evaluation,
            "status": if d.draft { "draft" } else { "published" },
            "type": if d.full { "full" } else { "short" },
            "user_rating": [{"label": "Итоговая оценка", "value": d.score.clamp(1, 10)}],
            "metadata": {"settings": {"disable_comments": false, "disable_votes": false}},
            "reviewable_type": d.site.catalog_path(),
            "reviewable_id": d.title_id,
            "content": d.text.to_prosemirror(),
            "related_media": [],
        })
    }

    /// Пишет отзыв. Возвращает его id.
    pub async fn create_review(&self, draft: &ReviewDraft) -> Result<u64> {
        let created: Value = self
            .send_data(
                draft.site,
                Method::POST,
                "reviews",
                &(),
                Some(&Self::review_body(draft)),
            )
            .await?;
        Ok(created.get("id").and_then(Value::as_u64).unwrap_or_default())
    }

    pub async fn update_review(&self, id: u64, draft: &ReviewDraft) -> Result<()> {
        self.send_ok(
            draft.site,
            Method::PUT,
            &format!("reviews/{id}"),
            Some(&Self::review_body(draft)),
        )
        .await
    }

    pub async fn delete_review(&self, id: u64) -> Result<()> {
        self.send_ok::<()>(Site::MangaLib, Method::DELETE, &format!("reviews/{id}"), None)
            .await
    }

    pub async fn vote_review(&self, id: u64, vote: Vote) -> Result<()> {
        self.vote("review", id, vote).await
    }

    // --- Озвучка манги ---

    /// Озвучки манги: видео, где читают вслух несколько глав. Только по `slug_url`,
    /// по числовому id сервер отвечает 404.
    pub async fn manga_voices(&self, site: Site, slug_url: &str, page: u32) -> Result<Page<MangaVoice>> {
        self.get_page(
            site,
            &format!("manga/{slug_url}/chapters/players"),
            &[("page", page.max(1))],
        )
        .await
    }

    /// Адрес видео озвучки манги. Серверы у озвучек свои (`videoServersManga`),
    /// без кириллического `/.аs/` в пути, в отличие от аниме.
    pub async fn manga_voice_url(&self, voice: &MangaVoice, max_quality: u64) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Servers {
            #[serde(default, rename = "videoServersManga", deserialize_with = "null_default")]
            servers: Vec<crate::models::MediaServer>,
        }
        let Some(quality) = voice.video.as_ref().and_then(|v| v.pick(max_quality)) else {
            return Ok(None);
        };
        let servers: Servers = self
            .get_data(Site::MangaLib, "constants", &[("fields[]", "videoServersManga")])
            .await?;
        Ok(servers.servers.first().map(|s| s.join(&quality.href)))
    }

    /// Отмечает озвучку манги просмотренной.
    pub async fn mark_manga_voice_viewed(&self, title_id: u64, voice_id: u64) -> Result<()> {
        self.send_ok::<()>(
            Site::MangaLib,
            Method::POST,
            &format!("manga/{title_id}/players/{voice_id}/view"),
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_with_mixed_ids() {
        let s: TitleStats = serde_json::from_value(json!({
            "bookmarks": {"count": 10, "stats": [
                {"percent": 28.1, "value": 3, "label": "Читаю", "meta": {"id": 1}},
                {"percent": 2.3, "value": 1, "label": "Другое", "meta": {"id": "other"}}
            ]},
            "rating": {"count": 5, "stats": [{"label": 10, "value": 5, "percent": 100, "meta": {"id": 10}}]}
        }))
        .unwrap();
        assert_eq!(s.bookmarks.stats[1].label, "Другое");
        assert_eq!(s.rating.stats[0].label, "10");
    }
}
