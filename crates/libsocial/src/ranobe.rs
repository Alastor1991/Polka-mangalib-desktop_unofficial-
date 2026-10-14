//! Ранобэ (RanobeLib, `Site-Id: 3`): текст главы.
//!
//! Ранобэ живёт в том же API, что и манга: пути `/manga/...`, в ответах `model` равен
//! `"manga"`. У главы нет `pages`, вместо них `content`, и он бывает двух видов, причём
//! оба встречаются в одном тайтле: старые главы приходят строкой HTML (только `<p>` и
//! `<img>`), новые документом ProseMirror. Оба вида разбираются в `rich::Doc`.
//!
//! Ловушки:
//!
//! - у старых глав `attachments[].url` строится по нынешнему slug тайтла, а файл лежит
//!   под старым, так что адрес даёт 404 или 500. Рабочий адрес стоит в `src` самого
//!   HTML, поэтому для HTML-глав картинки берутся только оттуда. У новых глав путь
//!   строится по числовому id тайтла и рабочий, а в документе картинка названа только
//!   именем вложения, адрес надо искать в `attachments`;
//! - у каждого абзаца есть номер, нумерация общая по документу, с единицы, в том числе
//!   для абзацев внутри цитат и списков. Сайт пишет этот номер в закладку (`progress`) и
//!   прокручивает к нему по ссылке `?p=N`. `rich::Doc` номеров не хранит, поэтому глава
//!   держит их рядом: `spans` параллелен `doc.blocks`;
//! - пустой абзац нового вида – это `paragraph` без текста или только с `hardBreak`, он
//!   тоже получает номер;
//! - запросы идут с явным `Site-Id: 3`, потому что `Site::RanobeLib` в крейте пока нет.

use reqwest::Method;
use reqwest::header::HeaderMap;

use crate::site::Site;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bookmarks::Place;
use crate::client::{Auth, Client, join_url};
use crate::error::{Error, Result};
use crate::models::{Branch, Chapter, Data, Team};
use crate::rich::{Block, Doc, Inline, Picture};
use crate::serde_util::{lenient_vec, null_default, opt_u64_lenient, string_lenient, u64_lenient};

/// Домен сайта. Картинки глав лежат на нём же, а `Referer` у них проверяет любой домен сети.
pub const RANOBE_ORIGIN: &str = "https://ranobelib.me";

/// Заголовки сайта RanobeLib: `Site-Id`, `Referer`, `Origin`.
pub(crate) fn ranobe_headers() -> HeaderMap {
    Client::site_headers(Site::RanobeLib)
}

/// В каком виде сервер прислал текст главы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentFormat {
    /// Строка HTML: старые главы.
    Html,
    /// Документ ProseMirror: новые главы.
    ProseMirror,
}

/// Картинка из `attachments`. Адрес `url` относительный, от домена сайта.
///
/// Верить ему можно только у глав с документом ProseMirror. У HTML-глав он строится по
/// нынешнему slug тайтла и часто не открывается, там нужен адрес из `src`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChapterAttachment {
    /// Имя вложения без расширения. На него ссылается узел `image` документа.
    #[serde(default, deserialize_with = "string_lenient")]
    pub name: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub filename: String,
    #[serde(default, deserialize_with = "string_lenient")]
    pub extension: String,
    /// Размеры у старых глав нулевые.
    #[serde(default, deserialize_with = "u64_lenient")]
    pub width: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    pub height: u64,
    #[serde(default, deserialize_with = "string_lenient")]
    pub url: String,
    #[serde(default, rename = "previewUrl")]
    pub preview_url: Option<String>,
    #[serde(default, rename = "originalUrl")]
    pub original_url: Option<String>,
}

impl ChapterAttachment {
    /// Полный адрес оригинала. Для HTML-глав может не открываться, см. описание типа.
    pub fn original_address(&self) -> String {
        absolute_url(
            self.original_url
                .as_deref()
                .filter(|u| !u.is_empty())
                .unwrap_or(&self.url),
        )
    }
}

/// Какие номера абзацев лежат в одном блоке `Doc`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParagraphSpan {
    /// Номер первого абзаца блока, с единицы. Нуль, если в блоке абзацев нет.
    pub first: u32,
    /// Сколько абзацев в блоке: один у обычного, несколько у цитаты и списка.
    pub count: u32,
}

impl ParagraphSpan {
    pub const NONE: ParagraphSpan = ParagraphSpan { first: 0, count: 0 };

    pub fn contains(&self, paragraph: u32) -> bool {
        self.count > 0 && paragraph >= self.first && paragraph - self.first < self.count
    }
}

/// Глава ранобэ: заголовок и разобранный текст.
#[derive(Debug, Clone)]
pub struct RanobeChapter {
    /// Id перевода в ветке. Он нужен для закладки, отметки прочтения и комментариев.
    pub id: u64,
    /// Числовой id тайтла.
    pub manga_id: Option<u64>,
    pub volume: String,
    pub number: String,
    pub number_secondary: String,
    /// Пустая строка, если у главы нет названия.
    pub name: String,
    pub branch_id: Option<u64>,
    pub created_at: Option<String>,
    pub likes_count: u64,
    /// Лайк и прочтение приходят только вошедшему пользователю.
    pub is_liked: bool,
    pub is_viewed: bool,
    pub teams: Vec<Team>,
    pub format: ContentFormat,
    /// Сырой `content` как есть: строка или документ.
    pub content: Value,
    /// Текст главы. Картинки в нём уже с полными адресами.
    pub doc: Doc,
    /// Номера абзацев по блокам: параллелен `doc.blocks`.
    pub spans: Vec<ParagraphSpan>,
    pub attachments: Vec<ChapterAttachment>,
}

#[derive(Deserialize)]
struct RawChapter {
    #[serde(deserialize_with = "u64_lenient")]
    id: u64,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    manga_id: Option<u64>,
    #[serde(default, deserialize_with = "string_lenient")]
    volume: String,
    #[serde(default, deserialize_with = "string_lenient")]
    number: String,
    #[serde(default, deserialize_with = "string_lenient")]
    number_secondary: String,
    #[serde(default, deserialize_with = "string_lenient")]
    name: String,
    #[serde(default, deserialize_with = "opt_u64_lenient")]
    branch_id: Option<u64>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default, deserialize_with = "u64_lenient")]
    likes_count: u64,
    #[serde(default, deserialize_with = "null_default")]
    is_liked: bool,
    #[serde(default, deserialize_with = "null_default")]
    is_viewed: bool,
    #[serde(default, deserialize_with = "null_default")]
    teams: Vec<Team>,
    #[serde(default)]
    content: Value,
    #[serde(default, deserialize_with = "lenient_vec")]
    attachments: Vec<ChapterAttachment>,
}

impl RanobeChapter {
    /// Разбирает объект `data` ответа `GET /manga/{slug}/chapter`.
    pub fn from_value(data: &Value) -> Result<RanobeChapter> {
        let raw: RawChapter = serde_json::from_value(data.clone()).map_err(|source| Error::Decode {
            context: "ранобэ: глава".into(),
            source,
        })?;
        let (format, doc, spans) = parse_content(&raw.content, &raw.attachments);
        Ok(RanobeChapter {
            id: raw.id,
            manga_id: raw.manga_id,
            volume: raw.volume,
            number: raw.number,
            number_secondary: raw.number_secondary,
            name: raw.name,
            branch_id: raw.branch_id,
            created_at: raw.created_at,
            likes_count: raw.likes_count,
            is_liked: raw.is_liked,
            is_viewed: raw.is_viewed,
            teams: raw.teams,
            format,
            content: raw.content,
            doc,
            spans,
            attachments: raw.attachments,
        })
    }

    /// Подпись главы: «Том 1 Глава 0 – Пролог».
    pub fn title(&self) -> String {
        let base = format!("Том {} Глава {}", self.volume, self.number);
        match self.name.trim() {
            "" => base,
            name => format!("{base} – {name}"),
        }
    }

    /// Номер последнего абзаца. Нуль, если абзацев нет.
    pub fn paragraph_count(&self) -> u32 {
        self.spans
            .iter()
            .filter(|s| s.count > 0)
            .map(|s| s.first + s.count - 1)
            .max()
            .unwrap_or(0)
    }

    /// Индекс блока `doc.blocks`, в котором лежит абзац с этим номером. Если такого
    /// номера нет (пустой абзац старой главы сервер мог не прислать), берётся ближайший
    /// блок перед ним. `None`, если абзацев нет или номер раньше первого.
    pub fn block_of_paragraph(&self, paragraph: u32) -> Option<usize> {
        self.spans.iter().position(|s| s.contains(paragraph)).or_else(|| {
            self.spans
                .iter()
                .enumerate()
                .rfind(|(_, s)| s.count > 0 && s.first < paragraph)
                .map(|(i, _)| i)
        })
    }

    /// Все картинки главы по порядку появления, с готовыми полными адресами.
    pub fn pictures(&self) -> Vec<&Picture> {
        let mut out = Vec::new();
        collect_pictures(&self.doc.blocks, &mut out);
        out
    }

    /// Адреса картинок главы без повторов, для скачивания и офлайн-кэша. Картинку отдаёт
    /// `Client::fetch_bytes` с любым сайтом сети в `site`.
    pub fn image_urls(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.pictures()
            .into_iter()
            .map(|p| {
                p.original
                    .clone()
                    .filter(|o| !o.is_empty())
                    .unwrap_or_else(|| p.url.clone())
            })
            .filter(|u| !u.is_empty() && seen.insert(u.clone()))
            .collect()
    }

    /// Место для закладки на абзац. Сайт кладёт в `progress` номер абзаца строкой.
    /// Автозакладка при чтении `progress` не шлёт вовсе, этот метод для явной команды
    /// «закладка здесь». `item_number` – порядковый номер главы в тайтле, `Chapter::item_number`.
    pub fn bookmark_place(&self, item_number: Option<u64>, paragraph: u32) -> Place {
        Place {
            item_id: self.id,
            item_number,
            progress: paragraph.to_string(),
            player: None,
        }
    }
}

/// Номер абзаца из `progress` закладки ранобэ. `None` для пустого и нечислового значения.
pub fn paragraph_from_progress(progress: &str) -> Option<u32> {
    progress.trim().parse().ok().filter(|n| *n > 0)
}

/// Относительный адрес картинки превращает в полный. Абсолютные не трогает.
fn absolute_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        format!("{RANOBE_ORIGIN}/{}", url.trim_start_matches('/'))
    }
}

fn parse_content(content: &Value, attachments: &[ChapterAttachment]) -> (ContentFormat, Doc, Vec<ParagraphSpan>) {
    // Документ иногда упакован в строку.
    let packed;
    let content = match content {
        Value::String(s) if s.trim_start().starts_with('{') => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => {
                packed = v;
                &packed
            }
            _ => content,
        },
        _ => content,
    };
    match content {
        Value::Object(_) => {
            let (doc, spans) = build_prosemirror(content, attachments);
            (ContentFormat::ProseMirror, doc, spans)
        }
        Value::String(html) => {
            let (doc, spans) = build_html(html);
            (ContentFormat::Html, doc, spans)
        }
        _ => (ContentFormat::Html, Doc::default(), Vec::new()),
    }
}

// ---------------------------------------------------------------- ProseMirror

fn build_prosemirror(content: &Value, attachments: &[ChapterAttachment]) -> (Doc, Vec<ParagraphSpan>) {
    let mut blocks = Vec::new();
    let mut spans = Vec::new();
    let mut counter = 0u32;
    let nodes = content
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for node in nodes {
        // Считаем по исходному узлу: подмены ниже абзацев не добавляют.
        let count = count_paragraphs(node);
        let wrapped = json!({"type": "doc", "content": [resolve_node(node, attachments)]});
        let parsed = Doc::from_prosemirror(&wrapped).blocks;
        for (i, block) in parsed.into_iter().enumerate() {
            blocks.push(block);
            spans.push(if i == 0 && count > 0 {
                ParagraphSpan {
                    first: counter + 1,
                    count,
                }
            } else {
                ParagraphSpan::NONE
            });
        }
        counter += count;
    }
    (Doc { blocks }, spans)
}

fn count_paragraphs(node: &Value) -> u32 {
    let own = u32::from(node.get("type").and_then(Value::as_str) == Some("paragraph"));
    own + node
        .get("content")
        .and_then(Value::as_array)
        .map_or(0, |c| c.iter().map(count_paragraphs).sum())
}

/// Готовит узел для `Doc::from_prosemirror`: картинкам подставляет адреса из вложений,
/// кнопку превращает в ссылку, потому что `Doc` кнопок не знает.
fn resolve_node(node: &Value, attachments: &[ChapterAttachment]) -> Value {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "image" => resolve_image(node, attachments),
        "button" => {
            let attrs = node.get("attrs").unwrap_or(&Value::Null);
            let pick = |key: &str| {
                node.get(key)
                    .or_else(|| attrs.get(key))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let text = pick("text").or_else(|| pick("href")).unwrap_or_default();
            let marks =
                pick("href").map_or_else(Vec::new, |href| vec![json!({"type": "link", "attrs": {"href": href}})]);
            json!({"type": "paragraph", "content": [{"type": "text", "text": text, "marks": marks}]})
        }
        _ => {
            let mut out = node.clone();
            if let Some(children) = node.get("content").and_then(Value::as_array) {
                out["content"] = Value::Array(children.iter().map(|c| resolve_node(c, attachments)).collect());
            }
            out
        }
    }
}

fn resolve_image(node: &Value, attachments: &[ChapterAttachment]) -> Value {
    let attrs = node.get("attrs").unwrap_or(&Value::Null);
    let images: Vec<Value> = attrs
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.get("image").and_then(Value::as_str)?;
            // Вложение, которого нет в списке, пропускаем: имени вместо адреса не отдаём.
            let att = attachments.iter().find(|a| a.name == name || a.filename == name)?;
            let shown = att.preview_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(&att.url);
            Some(json!({
                "url": absolute_url(shown),
                "originalUrl": att.original_address(),
                "width": att.width,
                "height": att.height,
            }))
        })
        .collect();
    json!({"type": "image", "attrs": {"images": images}})
}

// ----------------------------------------------------------------------- HTML

fn build_html(html: &str) -> (Doc, Vec<ParagraphSpan>) {
    let mut blocks = Vec::new();
    let mut spans = Vec::new();
    for (number, chunk) in split_html(html) {
        for (i, block) in Doc::from_html(chunk).blocks.into_iter().enumerate() {
            blocks.push(block);
            spans.push(match number {
                Some(first) if i == 0 => ParagraphSpan { first, count: 1 },
                _ => ParagraphSpan::NONE,
            });
        }
    }
    fix_relative(&mut blocks);
    (Doc { blocks }, spans)
}

/// Режет HTML на абзацы `<p>` (с номером) и всё остальное между ними (без номера).
/// Номер берётся из `data-paragraph-index`, а если его нет, считается по порядку.
/// Сервер присылает только `<p>` и `<img>` верхнего уровня, вложенных `<p>` не бывает.
fn split_html(html: &str) -> Vec<(Option<u32>, &str)> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let (mut pos, mut from, mut seq) = (0usize, 0usize, 0u32);
    while let Some(rel) = lower[from..].find("<p") {
        let start = from + rel;
        let is_p = matches!(
            lower.as_bytes().get(start + 2),
            Some(b'>' | b' ' | b'\n' | b'\t' | b'\r' | b'/')
        );
        if !is_p {
            from = start + 2;
            continue;
        }
        let Some(tag_len) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + tag_len;
        let end = lower[open_end..].find("</p>").map_or(html.len(), |i| open_end + i + 4);
        if start > pos {
            out.push((None, &html[pos..start]));
        }
        seq = paragraph_index(&lower[start..=open_end]).unwrap_or(seq + 1);
        out.push((Some(seq), &html[start..end]));
        pos = end;
        from = end;
    }
    if pos < html.len() {
        out.push((None, &html[pos..]));
    }
    out
}

fn paragraph_index(open_tag: &str) -> Option<u32> {
    let rest = &open_tag[open_tag.find("data-paragraph-index")? + "data-paragraph-index".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|n| *n > 0)
}

// ------------------------------------------------------------------- картинки

fn fix_relative(blocks: &mut [Block]) {
    fn fix(p: &mut Picture) {
        if !p.url.is_empty() {
            p.url = absolute_url(&p.url);
        }
        if let Some(o) = &mut p.original {
            *o = absolute_url(o);
        }
    }
    fn inlines(items: &mut [Inline]) {
        for item in items {
            match item {
                Inline::Image(p) => fix(p),
                Inline::Spoiler { content, .. } => inlines(content),
                _ => {}
            }
        }
    }
    for block in blocks {
        match block {
            Block::Paragraph(items) | Block::Heading { content: items, .. } => inlines(items),
            Block::Quote(inner) | Block::Spoiler { blocks: inner, .. } => fix_relative(inner),
            Block::List { items, .. } => items.iter_mut().for_each(|i| fix_relative(i)),
            Block::Images(pictures) => pictures.iter_mut().for_each(fix),
            Block::Sticker(p) => fix(p),
            Block::TitleCard { .. } | Block::Rule => {}
        }
    }
}

fn collect_pictures<'a>(blocks: &'a [Block], out: &mut Vec<&'a Picture>) {
    fn inlines<'a>(items: &'a [Inline], out: &mut Vec<&'a Picture>) {
        for item in items {
            match item {
                Inline::Image(p) => out.push(p),
                Inline::Spoiler { content, .. } => inlines(content, out),
                _ => {}
            }
        }
    }
    for block in blocks {
        match block {
            Block::Paragraph(items) | Block::Heading { content: items, .. } => inlines(items, out),
            Block::Quote(inner) | Block::Spoiler { blocks: inner, .. } => collect_pictures(inner, out),
            Block::List { items, .. } => items.iter().for_each(|i| collect_pictures(i, out)),
            Block::Images(pictures) => out.extend(pictures),
            Block::Sticker(_) | Block::TitleCard { .. } | Block::Rule => {}
        }
    }
}

// --------------------------------------------------------------------- запросы

impl Client {
    /// Запрос к API от имени RanobeLib (`Site-Id: 3`). Ответ целиком, для бинарных тел.
    pub(crate) async fn ranobe_request<Q, B>(
        &self,
        method: Method,
        path: &str,
        query: &Q,
        body: Option<&B>,
    ) -> Result<reqwest::Response>
    where
        Q: Serialize + ?Sized,
        B: Serialize + ?Sized,
    {
        self.send_api(Auth::User, |http, base| {
            let request = http
                .request(method.clone(), join_url(base, path))
                .headers(ranobe_headers())
                .query(query);
            match body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await
    }

    /// Запрос от имени RanobeLib с разбором `{"data": T}`.
    pub(crate) async fn ranobe_data<T, Q, B>(
        &self,
        method: Method,
        path: &str,
        query: &Q,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
        B: Serialize + ?Sized,
    {
        let bytes = self.ranobe_request(method, path, query, body).await?.bytes().await?;
        let parsed: Data<T> = serde_json::from_slice(&bytes).map_err(|source| Error::Decode {
            context: path.to_string(),
            source,
        })?;
        Ok(parsed.data)
    }

    /// Глава ранобэ по тому и номеру из списка глав. `branch_id` берётся из
    /// `ChapterBranch::branch_id`; у части тайтлов он `null`, тогда передают `None`.
    /// Закрытая глава отвечает так же, как у манги: ошибкой.
    pub async fn ranobe_chapter(
        &self,
        slug_url: &str,
        volume: &str,
        number: &str,
        branch_id: Option<u64>,
    ) -> Result<RanobeChapter> {
        let mut query = vec![("volume", volume.to_string()), ("number", number.to_string())];
        if let Some(branch) = branch_id {
            query.push(("branch_id", branch.to_string()));
        }
        let data: Value = self
            .ranobe_data(Method::GET, &format!("manga/{slug_url}/chapter"), &query, None::<&()>)
            .await?;
        RanobeChapter::from_value(&data)
    }

    /// То же по главе из списка `ranobe_chapters`.
    pub async fn ranobe_chapter_of(
        &self,
        slug_url: &str,
        chapter: &Chapter,
        branch_id: Option<u64>,
    ) -> Result<RanobeChapter> {
        self.ranobe_chapter(slug_url, &chapter.volume, &chapter.number, branch_id)
            .await
    }

    /// Список глав ранобэ одним ответом. У лицензированных тайтлов он пуст. Главы
    /// сортируют по `Chapter::item_number`, а не по `number`: пролог бывает «0» или «0.1».
    pub async fn ranobe_chapters(&self, slug_url: &str) -> Result<Vec<Chapter>> {
        self.ranobe_data(Method::GET, &format!("manga/{slug_url}/chapters"), &(), None::<&()>)
            .await
    }

    /// Ветки перевода тайтла ранобэ.
    pub async fn ranobe_branches(&self, title_id: u64) -> Result<Vec<Branch>> {
        self.ranobe_data(Method::GET, &format!("branches/{title_id}"), &(), None::<&()>)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(name: &str, dir: &str) -> Value {
        json!({"name": name, "filename": format!("{name}.png"), "extension": "png", "width": 100, "height": 50,
               "url": format!("/uploads/ranobe/{dir}/{name}.png"),
               "previewUrl": format!("/uploads/ranobe/{dir}/{name}.png"),
               "originalUrl": format!("/uploads/ranobe/{dir}/{name}.png")})
    }

    #[test]
    fn html_numbers_and_images() {
        let html = concat!(
            "<img loading=\"lazy\" src=\"https://ranobelib.me/uploads/ranobe/old-slug/chapters/5/a.jpg\" />",
            "<p data-paragraph-index=\"1\">Пролог</p>",
            "<p data-paragraph-index=\"2\">строка\nс переносом &amp; ещё</p>",
            "<p data-paragraph-index=\"3\"></p>",
            "<img src=\"/uploads/ranobe/old-slug/chapters/5/b.jpg\">",
            "<p data-paragraph-index=\"4\">конец</p>",
        );
        let data = json!({"id": 5, "manga_id": 9, "volume": "1", "number": "0", "content": html,
            "attachments": [{"name": "a", "filename": "a.jpg", "extension": "jpg", "width": 0, "height": 0,
                             "url": "/uploads/ranobe/WRONG-slug/chapters/5/a.jpg"}]});
        let ch = RanobeChapter::from_value(&data).unwrap();
        assert_eq!(ch.format, ContentFormat::Html);
        assert_eq!(ch.doc.blocks.len(), ch.spans.len());
        // Адреса из src, а не из attachments.
        assert_eq!(
            ch.image_urls(),
            [
                "https://ranobelib.me/uploads/ranobe/old-slug/chapters/5/a.jpg",
                "https://ranobelib.me/uploads/ranobe/old-slug/chapters/5/b.jpg"
            ]
        );
        assert_eq!(ch.paragraph_count(), 4);
        let at = |n| ch.block_of_paragraph(n).map(|i| &ch.doc.blocks[i]);
        let Some(Block::Paragraph(inl)) = at(2) else {
            panic!("нет абзаца 2")
        };
        assert!(matches!(&inl[0], Inline::Text { text, .. } if text == "строка с переносом & ещё"));
        // Пустой абзац 3 остаётся своим блоком, чтобы номера не съезжали.
        assert_ne!(ch.block_of_paragraph(3), ch.block_of_paragraph(2));
        assert!(matches!(at(3), Some(Block::Paragraph(i)) if i.is_empty()));
        // Номера за концом главы ведут к последнему абзацу.
        assert_eq!(ch.block_of_paragraph(99), ch.block_of_paragraph(4));
        let text = ch.doc.plain_text();
        assert!(text.starts_with("Пролог\n\nстрока с переносом & ещё") && text.ends_with("конец"));
    }

    #[test]
    fn html_without_index_counts_in_order() {
        let (_, spans) = build_html("<p>один</p><p>два</p><p data-paragraph-index=\"7\">семь</p><p>восемь</p>");
        let firsts: Vec<u32> = spans.iter().map(|s| s.first).collect();
        assert_eq!(firsts, [1, 2, 7, 8]);
    }

    #[test]
    fn prosemirror_images_come_from_attachments() {
        let data = json!({"id": 6, "volume": "14", "number": "531",
            "content": {"type": "doc", "content": [
                {"type": "heading", "attrs": {"level": 3}, "content": [{"type": "text", "text": "Заголовок"}]},
                {"type": "paragraph", "content": [{"type": "text", "text": "один"}]},
                {"type": "image", "attrs": {"images": [{"image": "img-1"}], "description": null}},
                {"type": "paragraph", "content": [{"type": "hardBreak"}]},
                {"type": "blockquote", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "цитата", "marks": [{"type": "italic"}]}]},
                    {"type": "paragraph"}
                ]},
                {"type": "paragraph", "content": [{"type": "text", "text": "после"}]}
            ]},
            "attachments": [att("img-1", "14841/chapters/6")]});
        let ch = RanobeChapter::from_value(&data).unwrap();
        assert_eq!(ch.format, ContentFormat::ProseMirror);
        assert_eq!(ch.doc.blocks.len(), ch.spans.len());
        assert_eq!(
            ch.image_urls(),
            ["https://ranobelib.me/uploads/ranobe/14841/chapters/6/img-1.png"]
        );
        // Заголовок и картинка абзацев не добавляют: один, пустой, два в цитате, последний.
        assert_eq!(ch.paragraph_count(), 5);
        let quote = ch.block_of_paragraph(3).unwrap();
        assert!(matches!(ch.doc.blocks[quote], Block::Quote(_)));
        assert_eq!(ch.spans[quote], ParagraphSpan { first: 3, count: 2 });
        assert_eq!(ch.block_of_paragraph(4), Some(quote));
        let last = ch.block_of_paragraph(5).unwrap();
        assert!(
            matches!(&ch.doc.blocks[last], Block::Paragraph(i) if matches!(&i[0], Inline::Text { text, .. } if text == "после"))
        );
        let pic = ch.pictures()[0];
        assert_eq!((pic.width, pic.height), (Some(100), Some(50)));
    }

    #[test]
    fn prosemirror_image_without_attachment_is_dropped() {
        let data = json!({"id": 1, "content": {"type": "doc", "content": [
            {"type": "image", "attrs": {"images": [{"image": "нет-такого"}]}},
            {"type": "paragraph", "content": [{"type": "text", "text": "текст"}]}
        ]}, "attachments": []});
        let ch = RanobeChapter::from_value(&data).unwrap();
        assert!(ch.image_urls().is_empty());
        assert_eq!(ch.doc.blocks.len(), ch.spans.len());
        assert_eq!(ch.block_of_paragraph(1), Some(0));
    }

    #[test]
    fn prosemirror_packed_into_string() {
        let packed =
            json!({"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "я"}]}]})
                .to_string();
        let ch = RanobeChapter::from_value(&json!({"id": 1, "content": packed})).unwrap();
        assert_eq!(ch.format, ContentFormat::ProseMirror);
        assert_eq!(ch.doc.plain_text(), "я");
    }

    #[test]
    fn button_becomes_link_without_number() {
        let data = json!({"id": 1, "content": {"type": "doc", "content": [
            {"type": "button", "attrs": {"text": "Читать", "href": "https://example.org"}},
            {"type": "paragraph", "content": [{"type": "text", "text": "текст"}]}
        ]}});
        let ch = RanobeChapter::from_value(&data).unwrap();
        assert_eq!(ch.doc.blocks.len(), 2);
        assert_eq!(ch.spans[0], ParagraphSpan::NONE);
        assert_eq!(ch.spans[1], ParagraphSpan { first: 1, count: 1 });
        let Block::Paragraph(inl) = &ch.doc.blocks[0] else {
            panic!("кнопка не стала абзацем")
        };
        let Inline::Text { marks, .. } = &inl[0] else { panic!() };
        assert_eq!(marks.link.as_deref(), Some("https://example.org"));
    }

    #[test]
    fn empty_or_missing_content() {
        let ch = RanobeChapter::from_value(&json!({"id": 1, "content": null})).unwrap();
        assert!(ch.doc.is_empty());
        assert_eq!(ch.paragraph_count(), 0);
        assert_eq!(ch.block_of_paragraph(1), None);
    }

    #[test]
    fn progress_and_place() {
        assert_eq!(paragraph_from_progress(" 17 "), Some(17));
        assert_eq!(paragraph_from_progress(""), None);
        assert_eq!(paragraph_from_progress("0"), None);
        let ch = RanobeChapter::from_value(&json!({"id": 42, "content": ""})).unwrap();
        let place = ch.bookmark_place(Some(3), 17);
        assert_eq!(
            (place.item_id, place.progress.as_str(), place.item_number),
            (42, "17", Some(3))
        );
    }

    #[test]
    fn title_formatting() {
        let ch = RanobeChapter::from_value(&json!({"id": 1, "volume": 1, "number": "0", "name": " Пролог "})).unwrap();
        assert_eq!(ch.title(), "Том 1 Глава 0 – Пролог");
        let ch = RanobeChapter::from_value(&json!({"id": 1, "volume": "2", "number": "5", "name": ""})).unwrap();
        assert_eq!(ch.title(), "Том 2 Глава 5");
    }
}
