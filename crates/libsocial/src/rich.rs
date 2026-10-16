//! Форматированный текст сервиса в одной модели. Сервер присылает текст в двух видах:
//! HTML (комментарии) и документ редактора ProseMirror (описания, главы ранобэ,
//! подсказки). Отправляет на сервер клиент только ProseMirror. Интерфейс рисует
//! `Doc` и не разбирает ни HTML, ни JSON сам.

use serde_json::{Value, json};

/// Документ: последовательность блоков.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Quote(Vec<Block>),
    /// Скрытый текст: виден заголовок, содержимое раскрывается по нажатию.
    Spoiler {
        title: String,
        blocks: Vec<Block>,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    /// Картинка или галерея из нескольких.
    Images(Vec<Picture>),
    Sticker(Picture),
    /// Карточка тайтла со ссылкой.
    TitleCard {
        href: String,
        title: String,
    },
    Rule,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Picture {
    /// Что показывать (превью, если есть).
    pub url: String,
    /// Оригинал для просмотрщика.
    pub original: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Оформление куска текста.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text {
        text: String,
        marks: Marks,
    },
    Break,
    Spoiler {
        title: String,
        content: Vec<Inline>,
    },
    /// Время в серии (`12:34`), по нажатию переход к нему.
    Timecode {
        text: String,
        secs: u64,
    },
    Image(Picture),
}

impl Doc {
    /// Текст без оформления: абзацы через пустую строку.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            block_text(block, &mut out);
        }
        out.trim().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.plain_text().is_empty()
            && !self
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Images(_) | Block::Sticker(_) | Block::TitleCard { .. }))
    }

    /// Документ из обычного текста: пустая строка разделяет абзацы, перевод строки
    /// внутри абзаца становится переносом.
    pub fn from_plain(text: &str) -> Doc {
        let text = text.replace("\r\n", "\n");
        let blocks = text
            .split("\n\n")
            .map(|p| p.trim_matches('\n'))
            .filter(|p| !p.trim().is_empty())
            .map(|p| {
                let mut content = Vec::new();
                for (i, line) in p.split('\n').enumerate() {
                    if i > 0 {
                        content.push(Inline::Break);
                    }
                    if !line.is_empty() {
                        content.push(Inline::Text {
                            text: line.to_string(),
                            marks: Marks::default(),
                        });
                    }
                }
                Block::Paragraph(content)
            })
            .collect();
        Doc { blocks }
    }

    /// Документ ProseMirror в том виде, в каком его принимает сервер.
    pub fn to_prosemirror(&self) -> Value {
        json!({"type": "doc", "content": self.blocks.iter().filter_map(block_json).collect::<Vec<_>>()})
    }

    /// Разбирает документ ProseMirror. Принимает и документ, упакованный в строку.
    pub fn from_prosemirror(value: &Value) -> Doc {
        let value = match value {
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(v @ Value::Object(_)) => v,
                _ => return Doc::from_plain(s),
            },
            v => v.clone(),
        };
        Doc {
            blocks: pm_blocks(value.get("content")),
        }
    }

    /// Разбирает документ редактора Quill (`{"ops": [...]}`). В нём хранятся старые
    /// темы форума, до перехода сайта на ProseMirror.
    pub fn from_quill(value: &Value) -> Doc {
        let mut lines = QuillLines::default();
        for op in value.get("ops").and_then(Value::as_array).into_iter().flatten() {
            let attrs = op.get("attributes").unwrap_or(&Value::Null);
            match op.get("insert") {
                Some(Value::String(text)) => {
                    // Перевод строки закрывает строку, и оформление строки (заголовок,
                    // список, цитата) лежит в атрибутах именно этой вставки.
                    let marks = quill_marks(attrs);
                    let mut parts = text.split('\n');
                    if let Some(first) = parts.next() {
                        lines.text(first, &marks);
                    }
                    for part in parts {
                        lines.end_line(attrs);
                        lines.text(part, &marks);
                    }
                }
                Some(embed @ Value::Object(_)) => {
                    if let Some(picture) = quill_picture(embed) {
                        lines.inlines.push(Inline::Image(picture));
                    }
                }
                _ => {}
            }
        }
        lines.finish()
    }

    /// Разбирает HTML, который сервер присылает в комментариях и тексте глав.
    pub fn from_html(html: &str) -> Doc {
        let nodes = parse_html(html);
        let mut blocks = Vec::new();
        html_blocks(&nodes, &mut blocks);
        Doc { blocks }
    }
}

fn block_text(block: &Block, out: &mut String) {
    let para = |inlines: &[Inline], out: &mut String| {
        inline_text(inlines, out);
        out.push_str("\n\n");
    };
    match block {
        Block::Paragraph(c) | Block::Heading { content: c, .. } => para(c, out),
        Block::Quote(blocks) | Block::Spoiler { blocks, .. } => blocks.iter().for_each(|b| block_text(b, out)),
        Block::List { items, .. } => items.iter().flatten().for_each(|b| block_text(b, out)),
        Block::TitleCard { title, .. } => para(
            &[Inline::Text {
                text: title.clone(),
                marks: Marks::default(),
            }],
            out,
        ),
        Block::Images(_) | Block::Sticker(_) | Block::Rule => {}
    }
}

fn inline_text(inlines: &[Inline], out: &mut String) {
    for i in inlines {
        match i {
            Inline::Text { text, .. } | Inline::Timecode { text, .. } => out.push_str(text),
            Inline::Break => out.push('\n'),
            Inline::Spoiler { content, .. } => inline_text(content, out),
            Inline::Image(_) => {}
        }
    }
}

// --- ProseMirror → Doc ---

fn pm_blocks(content: Option<&Value>) -> Vec<Block> {
    let Some(Value::Array(nodes)) = content else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for node in nodes {
        let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
        let attrs = node.get("attrs").cloned().unwrap_or(Value::Null);
        let children = node.get("content");
        match kind {
            "paragraph" => out.push(Block::Paragraph(pm_inlines(children, &Marks::default()))),
            "heading" => out.push(Block::Heading {
                level: attrs.get("level").and_then(Value::as_u64).unwrap_or(2).clamp(1, 6) as u8,
                content: pm_inlines(children, &Marks::default()),
            }),
            "blockquote" => out.push(Block::Quote(pm_blocks(children))),
            "spoilerBlock" => out.push(Block::Spoiler {
                title: str_attr(&attrs, "visibleText").unwrap_or_else(|| "спойлер".into()),
                blocks: pm_blocks(children),
            }),
            "bulletList" | "orderedList" => out.push(Block::List {
                ordered: kind == "orderedList",
                items: children
                    .and_then(Value::as_array)
                    .map(|items| items.iter().map(|i| pm_blocks(i.get("content"))).collect())
                    .unwrap_or_default(),
            }),
            "horizontalRule" => out.push(Block::Rule),
            "image" => {
                let pictures = pm_pictures(&attrs);
                if !pictures.is_empty() {
                    out.push(Block::Images(pictures));
                }
            }
            "sticker" => {
                if let Some(s) = attrs.get("stickers").and_then(Value::as_array).and_then(|a| a.first()) {
                    out.push(Block::Sticker(Picture {
                        url: sticker_url(s),
                        ..Picture::default()
                    }));
                }
            }
            "titleAttachment" => out.push(Block::TitleCard {
                href: str_attr(&attrs, "slug").unwrap_or_default(),
                title: str_attr(&attrs, "title").unwrap_or_default(),
            }),
            // Неизвестный блок: берём хотя бы текст.
            _ => {
                let inner = pm_blocks(children);
                if inner.is_empty() {
                    let inlines = pm_inlines(children, &Marks::default());
                    if !inlines.is_empty() {
                        out.push(Block::Paragraph(inlines));
                    }
                } else {
                    out.extend(inner);
                }
            }
        }
    }
    out
}

fn pm_inlines(content: Option<&Value>, base: &Marks) -> Vec<Inline> {
    let Some(Value::Array(nodes)) = content else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for node in nodes {
        let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut marks = base.clone();
        let mut timecode = false;
        for mark in node.get("marks").and_then(Value::as_array).into_iter().flatten() {
            match mark.get("type").and_then(Value::as_str).unwrap_or_default() {
                "bold" => marks.bold = true,
                "italic" => marks.italic = true,
                "underline" => marks.underline = true,
                "strike" => marks.strike = true,
                "link" => marks.link = mark.get("attrs").and_then(|a| str_attr(a, "href")),
                "timecode" => timecode = true,
                _ => {}
            }
        }
        match kind {
            "text" => {
                let text = node.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                match parse_clock(&text).filter(|_| timecode) {
                    Some(secs) => out.push(Inline::Timecode { text, secs }),
                    None => out.push(Inline::Text { text, marks }),
                }
            }
            "hardBreak" => out.push(Inline::Break),
            "spoilerInline" => out.push(Inline::Spoiler {
                title: node
                    .get("attrs")
                    .and_then(|a| str_attr(a, "visibleText"))
                    .unwrap_or_else(|| "спойлер".into()),
                content: pm_inlines(node.get("content"), &marks),
            }),
            "image" => {
                for p in pm_pictures(node.get("attrs").unwrap_or(&Value::Null)) {
                    out.push(Inline::Image(p));
                }
            }
            _ => out.extend(pm_inlines(node.get("content"), &marks)),
        }
    }
    out
}

fn pm_pictures(attrs: &Value) -> Vec<Picture> {
    if let Some(src) = str_attr(attrs, "src") {
        return vec![Picture {
            url: src,
            ..Picture::default()
        }];
    }
    attrs
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let url = str_attr(i, "url").or_else(|| str_attr(i, "image"))?;
            Some(Picture {
                url,
                original: str_attr(i, "originalUrl"),
                width: i.get("width").and_then(Value::as_u64).map(|w| w as u32),
                height: i.get("height").and_then(Value::as_u64).map(|h| h as u32),
            })
        })
        .collect()
}

fn sticker_url(s: &Value) -> String {
    match (
        s.get("sticker_pack_id").and_then(Value::as_u64),
        s.get("id").and_then(Value::as_u64),
        str_attr(s, "name"),
        str_attr(s, "extension"),
    ) {
        (Some(pack), Some(id), Some(name), Some(ext)) => format!("/uploads/stickers/{pack}/{id}/{name}.{ext}"),
        _ => String::new(),
    }
}

fn str_attr(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `12:34` или `1:02:03` в секунды.
fn parse_clock(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    parts
        .iter()
        .try_fold(0u64, |acc, p| Some(acc * 60 + p.parse::<u64>().ok()?))
}

// --- Quill → Doc ---

/// Строки документа Quill, из которых собираются блоки. Соседние строки одного
/// списка или одной цитаты сливаются в один блок.
#[derive(Default)]
struct QuillLines {
    blocks: Vec<Block>,
    inlines: Vec<Inline>,
    /// Список или цитата, в который пойдёт следующая строка того же вида.
    open: Option<QuillGroup>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuillGroup {
    List { ordered: bool },
    Quote,
}

impl QuillLines {
    fn text(&mut self, text: &str, marks: &Marks) {
        if !text.is_empty() {
            self.inlines.push(Inline::Text {
                text: text.to_string(),
                marks: marks.clone(),
            });
        }
    }

    fn end_line(&mut self, attrs: &Value) {
        let inlines = std::mem::take(&mut self.inlines);
        let group = match attrs.get("list").and_then(Value::as_str) {
            Some(kind) => Some(QuillGroup::List {
                ordered: kind == "ordered",
            }),
            None if matches!(attrs.get("blockquote"), Some(Value::Bool(true))) => Some(QuillGroup::Quote),
            None => None,
        };
        // Пустые строки в Quill – отступы между абзацами, блоком они не становятся.
        // Пустой пункт списка или строка цитаты свой блок не закрывает, иначе
        // нумерованный список начался бы заново.
        let blank = inlines
            .iter()
            .all(|i| matches!(i, Inline::Text { text, .. } if text.trim().is_empty()));
        if blank {
            if group.is_none() {
                self.open = None;
            }
            return;
        }
        let pictures: Vec<Picture> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Image(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        let block = if pictures.len() == inlines.len() {
            Block::Images(pictures)
        } else if let Some(level) = attrs.get("header").and_then(Value::as_u64) {
            Block::Heading {
                level: level.clamp(1, 6) as u8,
                content: inlines,
            }
        } else {
            Block::Paragraph(inlines)
        };
        let continues = group.is_some() && group == self.open;
        match (group, self.blocks.last_mut()) {
            (Some(QuillGroup::List { .. }), Some(Block::List { items, .. })) if continues => items.push(vec![block]),
            (Some(QuillGroup::Quote), Some(Block::Quote(blocks))) if continues => blocks.push(block),
            (Some(QuillGroup::List { ordered }), _) => self.blocks.push(Block::List {
                ordered,
                items: vec![vec![block]],
            }),
            (Some(QuillGroup::Quote), _) => self.blocks.push(Block::Quote(vec![block])),
            (None, _) => self.blocks.push(block),
        }
        self.open = group;
    }

    fn finish(mut self) -> Doc {
        // Последняя строка без перевода строки в конце.
        if !self.inlines.is_empty() {
            self.end_line(&Value::Null);
        }
        Doc { blocks: self.blocks }
    }
}

fn quill_marks(attrs: &Value) -> Marks {
    let on = |key: &str| attrs.get(key).and_then(Value::as_bool).unwrap_or(false);
    Marks {
        bold: on("bold"),
        italic: on("italic"),
        underline: on("underline"),
        strike: on("strike"),
        link: str_attr(attrs, "link"),
    }
}

/// Картинка Quill: `{"image": адрес}` или `{"image": {"src": …, "width": …}}`.
fn quill_picture(embed: &Value) -> Option<Picture> {
    let image = embed.get("image")?;
    let (url, attrs) = match image {
        Value::String(s) => (s.clone(), &Value::Null),
        other => (str_attr(other, "src")?, other),
    };
    if url.is_empty() {
        return None;
    }
    let size = |key: &str| {
        attrs
            .get(key)
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(Picture {
        url,
        original: None,
        width: size("width"),
        height: size("height"),
    })
}

// --- Doc → ProseMirror ---

fn block_json(block: &Block) -> Option<Value> {
    Some(match block {
        Block::Paragraph(c) => {
            let content = inlines_json(c);
            if content.is_empty() {
                json!({"type": "paragraph", "content": [{"type": "hardBreak"}]})
            } else {
                json!({"type": "paragraph", "content": content})
            }
        }
        Block::Heading { level, content } => {
            json!({"type": "heading", "attrs": {"level": level}, "content": inlines_json(content)})
        }
        Block::Quote(blocks) => json!({"type": "blockquote", "content": blocks_json(blocks)}),
        Block::Spoiler { title, blocks } => json!({
            "type": "spoilerBlock", "attrs": {"visibleText": title}, "content": blocks_json(blocks)
        }),
        Block::List { ordered, items } => json!({
            "type": if *ordered { "orderedList" } else { "bulletList" },
            "content": items.iter().map(|i| json!({"type": "listItem", "content": blocks_json(i)})).collect::<Vec<_>>()
        }),
        Block::Rule => json!({"type": "horizontalRule"}),
        // Картинки, стикеры и карточки в отправляемом тексте собирает тот, кто их
        // загрузил: им нужны имена файлов на сервере, а не адреса.
        Block::Images(_) | Block::Sticker(_) | Block::TitleCard { .. } => return None,
    })
}

fn blocks_json(blocks: &[Block]) -> Vec<Value> {
    blocks.iter().filter_map(block_json).collect()
}

fn inlines_json(inlines: &[Inline]) -> Vec<Value> {
    let mut out = Vec::new();
    for i in inlines {
        match i {
            Inline::Text { text, marks } if !text.is_empty() => {
                let mut node = json!({"type": "text", "text": text});
                let marks = marks_json(marks);
                if !marks.is_empty() {
                    node["marks"] = Value::Array(marks);
                }
                out.push(node);
            }
            Inline::Text { .. } | Inline::Image(_) => {}
            Inline::Break => out.push(json!({"type": "hardBreak"})),
            Inline::Spoiler { title, content } => out.push(json!({
                "type": "spoilerInline", "attrs": {"visibleText": title}, "content": inlines_json(content)
            })),
            Inline::Timecode { text, .. } => out.push(json!({"type": "text", "text": text})),
        }
    }
    out
}

fn marks_json(m: &Marks) -> Vec<Value> {
    let mut out = Vec::new();
    for (on, name) in [
        (m.bold, "bold"),
        (m.italic, "italic"),
        (m.underline, "underline"),
        (m.strike, "strike"),
    ] {
        if on {
            out.push(json!({"type": name}));
        }
    }
    if let Some(href) = &m.link {
        out.push(json!({"type": "link", "attrs": {"href": href}}));
    }
    out
}

// --- HTML ---

/// Узел разобранного HTML.
#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
    },
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        match self {
            Node::Element { attrs, .. } => attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()),
            Node::Text(_) => None,
        }
    }

    fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|c| c.split_whitespace().any(|x| x == class))
    }

    fn text(&self) -> String {
        match self {
            Node::Text(t) => t.clone(),
            Node::Element { children, .. } => children.iter().map(Node::text).collect(),
        }
    }
}

/// Открытый элемент при разборе: тег, атрибуты, уже разобранные дети.
type Open = (String, Vec<(String, String)>, Vec<Node>);

const VOID: &[&str] = &["br", "img", "hr", "input", "meta", "link", "source", "wbr"];
const BLOCK: &[&str] = &[
    "p",
    "div",
    "blockquote",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "figure",
    "section",
    "article",
];

/// Терпимый разбор HTML: незакрытые теги закрываются сами, лишние закрывающие
/// пропускаются. Скриптов и стилей сервер не присылает, но их содержимое всё равно
/// выбрасывается.
fn parse_html(html: &str) -> Vec<Node> {
    // Стек открытых элементов: тег, атрибуты, дети.
    let mut stack: Vec<Open> = vec![(String::new(), Vec::new(), Vec::new())];
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut stack, rest);
            break;
        };
        push_text(&mut stack, &rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |end| &rest[end + 3..]);
            continue;
        }
        let Some(gt) = tag_end(rest) else {
            push_text(&mut stack, rest);
            break;
        };
        let inner = &rest[1..gt];
        rest = &rest[gt + 1..];
        if let Some(name) = inner.strip_prefix('/') {
            let name = name.trim().to_ascii_lowercase();
            if let Some(pos) = stack.iter().rposition(|(t, ..)| *t == name) {
                while stack.len() > pos {
                    close(&mut stack);
                }
            }
            continue;
        }
        if inner.starts_with('!') || inner.starts_with('?') {
            continue;
        }
        let self_closing = inner.ends_with('/');
        let inner = inner.trim_end_matches('/');
        let (name, attrs) = parse_tag(inner);
        if name == "script" || name == "style" {
            let close_tag = format!("</{name}");
            rest = rest
                .to_ascii_lowercase()
                .find(&close_tag)
                .and_then(|i| rest[i..].find('>').map(|j| &rest[i + j + 1..]))
                .unwrap_or("");
            continue;
        }
        // <p> внутри <p> и подобное: блок закрывает открытый абзац.
        if BLOCK.contains(&name.as_str()) && stack.last().is_some_and(|(t, ..)| t == "p") {
            close(&mut stack);
        }
        if self_closing || VOID.contains(&name.as_str()) {
            stack.last_mut().expect("корень стека").2.push(Node::Element {
                tag: name,
                attrs,
                children: Vec::new(),
            });
        } else {
            stack.push((name, attrs, Vec::new()));
        }
    }
    while stack.len() > 1 {
        close(&mut stack);
    }
    stack.pop().map(|(.., c)| c).unwrap_or_default()
}

/// Конец тега с учётом `>` внутри кавычек атрибутов.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn push_text(stack: &mut [Open], text: &str) {
    if !text.is_empty() {
        stack
            .last_mut()
            .expect("корень стека")
            .2
            .push(Node::Text(decode_entities(text)));
    }
}

fn close(stack: &mut Vec<Open>) {
    let (tag, attrs, children) = stack.pop().expect("есть что закрывать");
    stack
        .last_mut()
        .expect("корень стека")
        .2
        .push(Node::Element { tag, attrs, children });
}

fn parse_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let inner = inner.trim();
    let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut rest = inner[name_end..].trim_start();
    while !rest.is_empty() {
        let key_end = rest.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (value, tail) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let end = body.find(q).unwrap_or(body.len());
                    (&body[..end], body.get(end + 1..).unwrap_or(""))
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..end], &after[end..])
                }
            };
            rest = tail.trim_start();
            decode_entities(value)
        } else {
            String::new()
        };
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    (name, attrs)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            e if e.starts_with("#x") || e.starts_with("#X") => {
                u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
            }
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Блоки из узлов HTML. Строчное содержимое между блоками собирается в абзацы.
fn html_blocks(nodes: &[Node], out: &mut Vec<Block>) {
    let mut pending: Vec<Inline> = Vec::new();
    let flush = |pending: &mut Vec<Inline>, out: &mut Vec<Block>| {
        let only_space = pending.iter().all(|i| match i {
            Inline::Text { text, .. } => text.trim().is_empty(),
            _ => false,
        });
        if !only_space {
            out.push(Block::Paragraph(std::mem::take(pending)));
        }
        pending.clear();
    };
    for node in nodes {
        let Node::Element { tag, children, .. } = node else {
            html_inline(node, &Marks::default(), &mut pending);
            continue;
        };
        let tag = tag.as_str();
        // Спойлер старого вида: заголовок и текст в отдельных span.
        if node.has_class("comment__spoiler") {
            flush(&mut pending, out);
            let title = find_class(children, "spoiler-title")
                .map(|n| n.text())
                .unwrap_or_else(|| "спойлер".into());
            let body = find_class(children, "spoiler-text")
                .map(|n| vec![n.clone()])
                .unwrap_or_default();
            let mut blocks = Vec::new();
            html_blocks(&body, &mut blocks);
            out.push(Block::Spoiler {
                title: title.trim().to_string(),
                blocks,
            });
            continue;
        }
        if node.has_class("spoiler-node") && node.attr("data-spoiler-type") == Some("block") {
            flush(&mut pending, out);
            let mut blocks = Vec::new();
            let body = find_class(children, "spoiler-node__text")
                .map(|n| match n {
                    Node::Element { children, .. } => children.clone(),
                    Node::Text(_) => vec![n.clone()],
                })
                .unwrap_or_else(|| children.clone());
            html_blocks(&body, &mut blocks);
            out.push(Block::Spoiler {
                title: node.attr("data-spoiler-text").unwrap_or("спойлер").to_string(),
                blocks,
            });
            continue;
        }
        if node.has_class("stickers-pack-node") || node.has_class("sticker-item-node") {
            flush(&mut pending, out);
            if let Some(img) = find_tag(children, "img") {
                out.push(Block::Sticker(picture(img)));
            }
            continue;
        }
        if node.has_class("comment-title-attachment") {
            flush(&mut pending, out);
            let link = find_tag(children, "a");
            let title = find_class(children, "comment-title-attachment__title")
                .map(|n| n.text())
                .unwrap_or_else(|| node.text());
            out.push(Block::TitleCard {
                href: link.and_then(|a| a.attr("href")).unwrap_or_default().to_string(),
                title: title.split_whitespace().collect::<Vec<_>>().join(" "),
            });
            continue;
        }
        match tag {
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                flush(&mut pending, out);
                let mut inlines = Vec::new();
                let mut inner_blocks = Vec::new();
                // Картинки-галереи внутри абзаца выносим отдельным блоком.
                for child in children {
                    html_inline(child, &Marks::default(), &mut inlines);
                }
                if let Some(level) = tag.strip_prefix('h').and_then(|l| l.parse().ok()) {
                    out.push(Block::Heading {
                        level,
                        content: inlines,
                    });
                } else if inlines.iter().all(|i| matches!(i, Inline::Image(_))) && !inlines.is_empty() {
                    inner_blocks.push(Block::Images(
                        inlines
                            .into_iter()
                            .filter_map(|i| match i {
                                Inline::Image(p) => Some(p),
                                _ => None,
                            })
                            .collect(),
                    ));
                    out.extend(inner_blocks);
                } else {
                    out.push(Block::Paragraph(inlines));
                }
            }
            "blockquote" => {
                flush(&mut pending, out);
                let mut inner = Vec::new();
                html_blocks(children, &mut inner);
                out.push(Block::Quote(inner));
            }
            "ul" | "ol" => {
                flush(&mut pending, out);
                let items = children
                    .iter()
                    .filter_map(|c| match c {
                        Node::Element { tag, children, .. } if tag == "li" => {
                            let mut item = Vec::new();
                            html_blocks(children, &mut item);
                            Some(item)
                        }
                        _ => None,
                    })
                    .collect();
                out.push(Block::List {
                    ordered: tag == "ol",
                    items,
                });
            }
            "hr" => {
                flush(&mut pending, out);
                out.push(Block::Rule);
            }
            "img" => {
                flush(&mut pending, out);
                out.push(Block::Images(vec![picture(node)]));
            }
            "div" | "section" | "article" | "figure" | "li" => {
                flush(&mut pending, out);
                html_blocks(children, out);
            }
            _ => html_inline(node, &Marks::default(), &mut pending),
        }
    }
    flush(&mut pending, out);
}

fn html_inline(node: &Node, marks: &Marks, out: &mut Vec<Inline>) {
    let (tag, children) = match node {
        Node::Text(text) => {
            // Переводы строк в HTML – просто пробелы.
            let text = text.replace(['\n', '\r', '\t'], " ");
            if !text.is_empty() {
                out.push(Inline::Text {
                    text,
                    marks: marks.clone(),
                });
            }
            return;
        }
        Node::Element { tag, children, .. } => (tag.as_str(), children),
    };
    if node.has_class("spoiler-node") || node.has_class("mark-spoiler") {
        let body = find_class(children, "spoiler-node__text")
            .map(|n| match n {
                Node::Element { children, .. } => children.clone(),
                Node::Text(_) => vec![n.clone()],
            })
            .unwrap_or_else(|| children.clone());
        let mut content = Vec::new();
        for c in &body {
            html_inline(c, marks, &mut content);
        }
        out.push(Inline::Spoiler {
            title: node.attr("data-spoiler-text").unwrap_or("спойлер").to_string(),
            content,
        });
        return;
    }
    if node.has_class("timecode-node") {
        let text = node.text();
        match parse_clock(&text) {
            Some(secs) => out.push(Inline::Timecode { text, secs }),
            None => out.push(Inline::Text {
                text,
                marks: marks.clone(),
            }),
        }
        return;
    }
    let mut marks = marks.clone();
    match tag {
        "br" => {
            out.push(Inline::Break);
            return;
        }
        "img" => {
            out.push(Inline::Image(picture(node)));
            return;
        }
        "b" | "strong" => marks.bold = true,
        "i" | "em" => marks.italic = true,
        "u" => marks.underline = true,
        "s" | "strike" | "del" => marks.strike = true,
        "a" => marks.link = node.attr("href").map(str::to_string),
        _ => {}
    }
    for child in children {
        html_inline(child, &marks, out);
    }
}

fn picture(node: &Node) -> Picture {
    let num = |name| node.attr(name).and_then(|v| v.parse().ok());
    Picture {
        url: node
            .attr("data-preview-src")
            .or(node.attr("src"))
            .unwrap_or_default()
            .to_string(),
        original: node.attr("data-original-src").map(str::to_string),
        width: num("data-width").or(num("width")),
        height: num("data-height").or(num("height")),
    }
}

fn find_class<'a>(nodes: &'a [Node], class: &str) -> Option<&'a Node> {
    for n in nodes {
        if n.has_class(class) {
            return Some(n);
        }
        if let Node::Element { children, .. } = n
            && let Some(found) = find_class(children, class)
        {
            return Some(found);
        }
    }
    None
}

fn find_tag<'a>(nodes: &'a [Node], tag: &str) -> Option<&'a Node> {
    for n in nodes {
        if let Node::Element { tag: t, children, .. } = n {
            if t == tag {
                return Some(n);
            }
            if let Some(found) = find_tag(children, tag) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text {
            text: s.into(),
            marks: Marks::default(),
        }
    }

    #[test]
    fn comment_html() {
        let doc = Doc::from_html(
            "<p>Привет &amp; <b>мир</b><br>строка</p><blockquote><p>цитата</p></blockquote>\
             <div class=\"comment__spoiler\"><span class=\"spoiler-title\">спойлер</span>\
             <span class=\"spoiler-text\">тайна</span></div>",
        );
        assert_eq!(doc.blocks.len(), 3);
        assert_eq!(
            doc.blocks[0],
            Block::Paragraph(vec![
                text("Привет & "),
                Inline::Text {
                    text: "мир".into(),
                    marks: Marks {
                        bold: true,
                        ..Marks::default()
                    }
                },
                Inline::Break,
                text("строка"),
            ])
        );
        assert!(matches!(&doc.blocks[1], Block::Quote(q) if q.len() == 1));
        assert!(matches!(&doc.blocks[2], Block::Spoiler { title, blocks } if title == "спойлер" && blocks.len() == 1));
        assert_eq!(doc.plain_text(), "Привет & мир\nстрока\n\nцитата\n\nтайна");
    }

    #[test]
    fn old_comment_without_paragraphs() {
        let doc = Doc::from_html("первая<br/>вторая <a href=\"https://x\">ссылка</a>");
        let Block::Paragraph(p) = &doc.blocks[0] else { panic!() };
        assert_eq!(p[0], text("первая"));
        assert_eq!(p[1], Inline::Break);
        assert!(matches!(&p[3], Inline::Text { marks, .. } if marks.link.as_deref() == Some("https://x")));
    }

    #[test]
    fn images_and_new_spoilers() {
        let doc = Doc::from_html(
            "<p><img src=\"https://c/x_preview.webp\" data-preview-src=\"https://c/x_preview.webp\" \
             data-original-src=\"https://c/x.jpg\" data-width=\"542\" data-height=\"400\"></p>\
             <p>a <span class=\"spoiler-node\" data-spoiler-type=\"inline\" data-spoiler-text=\"хм\">\
             <span class=\"spoiler-node__text\">b</span></span> \
             <span class=\"timecode-node\" data-player=\"Animelib\">12:34</span></p>",
        );
        let Block::Images(pics) = &doc.blocks[0] else {
            panic!("{doc:?}")
        };
        assert_eq!(pics[0].original.as_deref(), Some("https://c/x.jpg"));
        assert_eq!((pics[0].width, pics[0].height), (Some(542), Some(400)));
        let Block::Paragraph(p) = &doc.blocks[1] else { panic!() };
        assert!(
            p.iter().any(
                |i| matches!(i, Inline::Spoiler { title, content } if title == "хм" && content == &vec![text("b")])
            )
        );
        assert!(p.iter().any(|i| matches!(i, Inline::Timecode { secs: 754, .. })));
    }

    #[test]
    fn broken_html_does_not_panic() {
        for html in [
            "<p>a<b>b",
            "</p></div>x",
            "<a href='x",
            "&#99999999;&amp",
            "<p>a<p>b",
            "<script>x</script>y",
        ] {
            let _ = Doc::from_html(html).plain_text();
        }
        assert_eq!(Doc::from_html("<p>a<p>b").blocks.len(), 2);
        assert_eq!(Doc::from_html("<script>x</script>y").plain_text(), "y");
    }

    #[test]
    fn prosemirror_roundtrip() {
        let doc = Doc::from_plain("первый абзац\nвторая строка\n\nвторой");
        let json = doc.to_prosemirror();
        assert_eq!(json["type"], "doc");
        assert_eq!(json["content"][0]["content"][1]["type"], "hardBreak");
        assert_eq!(Doc::from_prosemirror(&json), doc);
        // Документ, упакованный в строку, как описание тайтла.
        let packed = Value::String(json.to_string());
        assert_eq!(Doc::from_prosemirror(&packed), doc);
    }

    #[test]
    fn prosemirror_marks_and_spoilers() {
        let json = serde_json::json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "жирно", "marks": [{"type": "bold"}]},
                {"type": "spoilerInline", "attrs": {"visibleText": "с"}, "content": [{"type": "text", "text": "x"}]},
                {"type": "text", "text": "1:02:03", "marks": [{"type": "timecode", "attrs": {"episode": 1}}]}
            ]},
            {"type": "spoilerBlock", "attrs": {"visibleText": "блок"}, "content": [{"type": "paragraph", "content": [{"type": "text", "text": "y"}]}]},
            {"type": "image", "attrs": {"images": [{"image": "uuid", "url": "https://c/u.jpg"}]}}
        ]});
        let doc = Doc::from_prosemirror(&json);
        let Block::Paragraph(p) = &doc.blocks[0] else { panic!() };
        assert!(matches!(&p[0], Inline::Text { marks, .. } if marks.bold));
        assert!(matches!(&p[2], Inline::Timecode { secs: 3723, .. }));
        assert!(matches!(&doc.blocks[1], Block::Spoiler { title, .. } if title == "блок"));
        assert!(matches!(&doc.blocks[2], Block::Images(p) if p[0].url == "https://c/u.jpg"));
        // Таймкод уходит обычным текстом: плеера и команды для метки модель не хранит.
        let back = Doc::from_prosemirror(&doc.to_prosemirror());
        let Block::Paragraph(b) = &back.blocks[0] else { panic!() };
        assert_eq!(b[..2], p[..2]);
        assert!(matches!(&b[2], Inline::Text { text, .. } if text == "1:02:03"));
    }

    #[test]
    fn quill_lines_and_groups() {
        let json = serde_json::json!({"ops": [
            {"insert": "Заголовок"},
            {"insert": "\n", "attributes": {"header": 2}},
            {"insert": "Первый абзац, "},
            {"insert": "жирно", "attributes": {"bold": true}},
            {"insert": " и "},
            {"insert": "ссылка", "attributes": {"link": "https://a.b/", "underline": true}},
            {"insert": "\n\nВторой абзац\nраз"},
            {"insert": "\n", "attributes": {"list": "bullet"}},
            {"insert": "два"},
            {"insert": "\n", "attributes": {"list": "bullet"}},
            {"insert": "цитата"},
            {"insert": "\n", "attributes": {"blockquote": true}},
            {"insert": {"image": {"src": "https://i.example/x.png", "width": "640"}}},
            {"insert": "\nхвост без перевода строки"}
        ]});
        let doc = Doc::from_quill(&json);
        assert!(matches!(&doc.blocks[0], Block::Heading { level: 2, content } if content == &vec![text("Заголовок")]));
        let Block::Paragraph(p) = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks[1])
        };
        assert_eq!(p.len(), 4);
        assert!(matches!(&p[1], Inline::Text { marks, .. } if marks.bold));
        assert!(
            matches!(&p[3], Inline::Text { marks, .. } if marks.underline && marks.link.as_deref() == Some("https://a.b/"))
        );
        // Пустая строка между абзацами блоком не стала.
        assert_eq!(doc.blocks[2], Block::Paragraph(vec![text("Второй абзац")]));
        let Block::List { ordered: false, items } = &doc.blocks[3] else {
            panic!("{:?}", doc.blocks[3])
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[1], vec![Block::Paragraph(vec![text("два")])]);
        assert_eq!(
            doc.blocks[4],
            Block::Quote(vec![Block::Paragraph(vec![text("цитата")])])
        );
        assert!(
            matches!(&doc.blocks[5], Block::Images(p) if p[0].url == "https://i.example/x.png" && p[0].width == Some(640))
        );
        assert_eq!(doc.blocks[6], Block::Paragraph(vec![text("хвост без перевода строки")]));
        assert_eq!(doc.blocks.len(), 7);
        assert!(Doc::from_quill(&serde_json::json!({"ops": []})).blocks.is_empty());
    }

    #[test]
    fn quill_ordered_list_survives_empty_item() {
        let json = serde_json::json!({"ops": [
            {"insert": "раз"}, {"insert": "\n", "attributes": {"list": "ordered"}},
            {"insert": "\n", "attributes": {"list": "ordered"}},
            {"insert": "два"}, {"insert": "\n", "attributes": {"list": "ordered"}},
            {"insert": "не цитата\n", "attributes": {"blockquote": false}},
            {"insert": "Заголовок\n", "attributes": {"header": 1}}
        ]});
        let doc = Doc::from_quill(&json);
        let Block::List { ordered: true, items } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks)
        };
        assert_eq!(items.len(), 2);
        assert_eq!(doc.blocks[1], Block::Paragraph(vec![text("не цитата")]));
        assert!(matches!(&doc.blocks[2], Block::Heading { level: 1, .. }));
        assert_eq!(doc.blocks.len(), 3);
    }
}
