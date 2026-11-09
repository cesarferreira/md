//! Owned Markdown document. Protobuf-free and terminal-free so it can be tested.

use std::path::Path;

use comrak::nodes::{AlertType, AstNode, ListType, NodeValue, TableAlignment};
use comrak::{Arena, Options, parse_document};

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub blocks: Vec<Section>,
}

/// A block plus the 1-based source lines it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub start_line: u32,
    pub end_line: u32,
    pub block: Block,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    /// Content inside a centred HTML container.
    Center(Vec<Section>),
    /// `<details>`: a summary line that expands to show its blocks.
    Details {
        summary: Vec<Inline>,
        open: bool,
        blocks: Vec<Section>,
    },
    Code {
        lang: String,
        literal: String,
    },
    Quote(Vec<Section>),
    Alert {
        kind: AlertKind,
        title: String,
        blocks: Vec<Section>,
    },
    List {
        ordered: bool,
        start: usize,
        items: Vec<Item>,
    },
    Table {
        aligns: Vec<Align>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    /// A paragraph made only of images, laid out in a row (badges, screenshots).
    Images(Vec<ImageRef>),
    Footnote {
        name: String,
        blocks: Vec<Section>,
    },
    FrontMatter(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub alt: String,
    pub dest: String,
    /// Where a click goes when the image is wrapped in a link.
    pub link: Option<String>,
    /// `width` from the HTML tag, when the author set one.
    pub width: Option<ImageWidth>,
}

/// An image's HTML `width`: a percentage of the column, or a CSS pixel size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageWidth {
    Px(f32),
    Percent(f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub checked: Option<bool>,
    pub blocks: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl AlertKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Tip => "Tip",
            Self::Important => "Important",
            Self::Warning => "Warning",
            Self::Caution => "Caution",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Code(String),
    SoftBreak,
    HardBreak,
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
    Link {
        dest: String,
        children: Vec<Inline>,
    },
    Image {
        alt: String,
        dest: String,
        width: Option<ImageWidth>,
    },
}

pub fn parse(markdown: &str) -> Document {
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.alerts = true;
    options.extension.front_matter_delimiter = Some("---".to_owned());
    options.parse.smart = true;
    let root = parse_document(&arena, markdown, &options);
    Document {
        blocks: blocks_of(root),
    }
}

pub fn plain(inlines: &[Inline]) -> String {
    let mut out = String::new();
    write_plain(inlines, &mut out);
    out
}

pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

pub fn is_markdown_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown" | "mdown" | "mkd")
    )
}

fn write_plain(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::SoftBreak => out.push(' '),
            Inline::HardBreak => out.push('\n'),
            Inline::Strong(children)
            | Inline::Emphasis(children)
            | Inline::Strike(children)
            | Inline::Link { children, dest: _ } => write_plain(children, out),
            Inline::Image { alt, .. } => out.push_str(alt),
        }
    }
}

enum OwnedBlock {
    Heading(u8),
    Paragraph,
    Code { lang: String, literal: String },
    Quote,
    Alert { kind: AlertKind, title: String },
    List { ordered: bool, start: usize },
    Table { aligns: Vec<Align> },
    Rule,
    Footnote(String),
    FrontMatter(String),
    Container,
    Skip,
}

fn blocks_of<'a>(node: &'a AstNode<'a>) -> Vec<Section> {
    // comrak ends an HTML block at every blank line, so one `<div align="center">`
    // can span several HTML siblings. Scan adjacent HTML blocks as one document.
    let mut nest = Nest::default();
    let mut html = String::new();
    let mut html_lines: Option<(u32, u32)> = None;
    let mut last_line = 0;
    for child in node.children() {
        let literal = match &child.data().value {
            NodeValue::HtmlBlock(block) => Some(block.literal.clone()),
            _ => None,
        };
        let (start, end) = lines_of(child);
        last_line = last_line.max(end);
        match literal {
            Some(literal) => {
                html.push_str(&literal);
                html_lines = Some(match html_lines {
                    Some((first, _)) => (first, end),
                    None => (start, end),
                });
            }
            None => {
                if let Some((first, last)) = html_lines.take() {
                    nest.html(&html, first, last);
                    html.clear();
                }
                if let Some(section) = section_of(child) {
                    nest.push(section);
                }
            }
        }
    }
    if let Some((first, last)) = html_lines {
        nest.html(&html, first, last);
    }
    nest.finish(last_line)
}

/// Groups everything between `<details>` and `</details>` into one block. The
/// tags usually sit in separate HTML blocks with Markdown between them.
#[derive(Default)]
struct Nest {
    root: Vec<Section>,
    open: Vec<OpenDetails>,
}

struct OpenDetails {
    summary: Vec<Inline>,
    open: bool,
    start_line: u32,
    blocks: Vec<Section>,
}

enum DetailsTag {
    Open { open: bool },
    Close,
}

impl Nest {
    fn push(&mut self, section: Section) {
        match self.open.last_mut() {
            Some(details) => details.blocks.push(section),
            None => self.root.push(section),
        }
    }

    fn html(&mut self, html: &str, start: u32, end: u32) {
        let mut rest = html;
        while let Some((before, tag, after)) = next_details_tag(rest) {
            for section in html_sections(before, start, end) {
                self.push(section);
            }
            rest = match tag {
                DetailsTag::Open { open } => {
                    let (summary, after) = take_summary(after);
                    self.open.push(OpenDetails {
                        summary,
                        open,
                        start_line: start,
                        blocks: Vec::new(),
                    });
                    after
                }
                DetailsTag::Close => {
                    self.close(end);
                    after
                }
            };
        }
        for section in html_sections(rest, start, end) {
            self.push(section);
        }
    }

    fn close(&mut self, end_line: u32) {
        let Some(details) = self.open.pop() else {
            return;
        };
        let summary = if details.summary.is_empty() {
            vec![Inline::Text("Details".to_owned())]
        } else {
            details.summary
        };
        self.push(Section {
            start_line: details.start_line,
            end_line: end_line.max(details.start_line),
            block: Block::Details {
                summary,
                open: details.open,
                blocks: details.blocks,
            },
        });
    }

    fn finish(mut self, end_line: u32) -> Vec<Section> {
        while !self.open.is_empty() {
            self.close(end_line);
        }
        self.root
    }
}

/// The next `<details>` or `</details>` tag: text before it, the tag, text after it.
fn next_details_tag(html: &str) -> Option<(&str, DetailsTag, &str)> {
    let lower = html.to_ascii_lowercase();
    let (index, closing) = match (lower.find("<details"), lower.find("</details")) {
        (Some(open), Some(close)) if close < open => (close, true),
        (Some(open), _) => (open, false),
        (None, Some(close)) => (close, true),
        (None, None) => return None,
    };
    let end = index + lower[index..].find('>')? + 1;
    let tag = if closing {
        DetailsTag::Close
    } else {
        DetailsTag::Open {
            open: lower[index..end].contains(" open"),
        }
    };
    Some((&html[..index], tag, &html[end..]))
}

/// A leading `<summary>…</summary>` as inlines, and the HTML after it.
fn take_summary(html: &str) -> (Vec<Inline>, &str) {
    let trimmed = html.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("<summary") {
        return (Vec::new(), html);
    }
    let (Some(open_end), Some(close)) = (lower.find('>'), lower.find("</summary")) else {
        return (Vec::new(), html);
    };
    if close < open_end {
        return (Vec::new(), html);
    }
    let inner = &trimmed[open_end + 1..close];
    let after = lower[close..]
        .find('>')
        .map_or("", |index| &trimmed[close + index + 1..]);
    (html_inlines(inner), after)
}

fn lines_of<'a>(node: &'a AstNode<'a>) -> (u32, u32) {
    let pos = node.data().sourcepos;
    (pos.start.line as u32, pos.end.line as u32)
}

fn section_of<'a>(node: &'a AstNode<'a>) -> Option<Section> {
    let (start_line, end_line) = lines_of(node);
    block_of(node).map(|block| Section {
        start_line,
        end_line: end_line.max(start_line),
        block,
    })
}

fn block_of<'a>(node: &'a AstNode<'a>) -> Option<Block> {
    let owned = {
        let data = node.data();
        match &data.value {
            NodeValue::Heading(heading) => OwnedBlock::Heading(heading.level),
            NodeValue::Paragraph => OwnedBlock::Paragraph,
            NodeValue::CodeBlock(code) => OwnedBlock::Code {
                lang: code.info.split_whitespace().next().unwrap_or("").to_owned(),
                literal: code.literal.clone(),
            },
            NodeValue::BlockQuote | NodeValue::MultilineBlockQuote(_) => OwnedBlock::Quote,
            NodeValue::Alert(alert) => {
                let kind = alert_kind(alert.alert_type);
                let title = alert
                    .title
                    .clone()
                    .unwrap_or_else(|| kind.label().to_owned());
                OwnedBlock::Alert { kind, title }
            }
            NodeValue::List(list) => OwnedBlock::List {
                ordered: matches!(list.list_type, ListType::Ordered),
                start: list.start.max(1),
            },
            NodeValue::Table(table) => OwnedBlock::Table {
                aligns: table
                    .alignments
                    .iter()
                    .map(|align| match align {
                        TableAlignment::Left => Align::Left,
                        TableAlignment::Center => Align::Center,
                        TableAlignment::Right => Align::Right,
                        TableAlignment::None => Align::None,
                    })
                    .collect(),
            },
            NodeValue::ThematicBreak => OwnedBlock::Rule,
            NodeValue::FootnoteDefinition(note) => OwnedBlock::Footnote(note.name.clone()),
            NodeValue::FrontMatter(matter) => OwnedBlock::FrontMatter(matter.clone()),
            NodeValue::DescriptionList
            | NodeValue::DescriptionItem(_)
            | NodeValue::DescriptionTerm
            | NodeValue::DescriptionDetails => OwnedBlock::Container,
            _ => OwnedBlock::Skip,
        }
    };

    match owned {
        OwnedBlock::Heading(level) => Some(Block::Heading {
            level,
            inlines: inlines_of(node),
        }),
        OwnedBlock::Paragraph => {
            let block = paragraph_or_images(inlines_of(node));
            match block {
                Block::Paragraph(inlines) if inlines.is_empty() => None,
                other => Some(other),
            }
        }
        OwnedBlock::Code { lang, literal } => Some(Block::Code { lang, literal }),
        OwnedBlock::Quote | OwnedBlock::Container => Some(Block::Quote(blocks_of(node))),
        OwnedBlock::Alert { kind, title } => Some(Block::Alert {
            kind,
            title,
            blocks: blocks_of(node),
        }),
        OwnedBlock::List { ordered, start } => Some(Block::List {
            ordered,
            start,
            items: list_items(node),
        }),
        OwnedBlock::Table { aligns } => Some(table_block(node, aligns)),
        OwnedBlock::Rule => Some(Block::Rule),
        OwnedBlock::Footnote(name) => Some(Block::Footnote {
            name,
            blocks: blocks_of(node),
        }),
        OwnedBlock::FrontMatter(matter) => Some(Block::FrontMatter(matter)),
        OwnedBlock::Skip => None,
    }
}

fn list_items<'a>(node: &'a AstNode<'a>) -> Vec<Item> {
    node.children()
        .filter_map(|child| {
            let checked = {
                let data = child.data();
                match &data.value {
                    NodeValue::TaskItem(task) => {
                        Some(task.symbol.is_some_and(|symbol| !symbol.is_whitespace()))
                    }
                    NodeValue::Item(_) => None,
                    _ => return None,
                }
            };
            Some(Item {
                checked,
                blocks: blocks_of(child),
            })
        })
        .collect()
}

fn table_block<'a>(node: &'a AstNode<'a>, aligns: Vec<Align>) -> Block {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    for row in node.children() {
        let is_header = matches!(row.data().value, NodeValue::TableRow(true));
        let cells = row.children().map(inlines_of).collect();
        if is_header && header.is_empty() {
            header = cells;
        } else {
            rows.push(cells);
        }
    }
    Block::Table {
        aligns,
        header,
        rows,
    }
}

fn paragraph_or_images(inlines: Vec<Inline>) -> Block {
    match image_row(&inlines) {
        Some(images) => Block::Images(images),
        None => Block::Paragraph(inlines),
    }
}

/// The images in a paragraph that holds nothing but images and whitespace.
fn image_row(inlines: &[Inline]) -> Option<Vec<ImageRef>> {
    let mut images = Vec::new();
    for inline in inlines {
        match inline {
            inline if is_blank(inline) => {}
            Inline::Image { alt, dest, width } => images.push(ImageRef {
                alt: alt.clone(),
                dest: dest.clone(),
                link: None,
                width: *width,
            }),
            Inline::Link {
                dest: link,
                children,
            } => {
                let mut parts = children.iter().filter(|child| !is_blank(child));
                let (Some(Inline::Image { alt, dest, width }), None) = (parts.next(), parts.next())
                else {
                    return None;
                };
                images.push(ImageRef {
                    alt: alt.clone(),
                    dest: dest.clone(),
                    link: Some(link.clone()),
                    width: *width,
                });
            }
            _ => return None,
        }
    }
    (!images.is_empty()).then_some(images)
}

fn is_blank(inline: &Inline) -> bool {
    match inline {
        Inline::SoftBreak | Inline::HardBreak => true,
        Inline::Text(text) => text.trim().is_empty(),
        _ => false,
    }
}

fn inlines_of<'a>(node: &'a AstNode<'a>) -> Vec<Inline> {
    let mut out = Vec::new();
    for child in node.children() {
        if matches!(child.data().value, NodeValue::Escaped) {
            out.extend(inlines_of(child));
            continue;
        }
        let html = {
            let data = child.data();
            match &data.value {
                NodeValue::HtmlInline(html) => Some(html.clone()),
                _ => None,
            }
        };
        if let Some(html) = html {
            out.extend(html_inlines(&html));
            continue;
        }
        if let Some(inline) = inline_of(child) {
            out.push(inline);
        }
    }
    out
}

enum OwnedInline {
    Text(String),
    Code(String),
    SoftBreak,
    HardBreak,
    Footnote { dest: String, label: String },
    Link(String),
    Image(String),
    Emphasis,
    Strong,
    Strike,
    Skip,
}

fn inline_of<'a>(node: &'a AstNode<'a>) -> Option<Inline> {
    let owned = {
        let data = node.data();
        match &data.value {
            NodeValue::Text(text) => OwnedInline::Text(text.to_string()),
            NodeValue::Code(code) => OwnedInline::Code(code.literal.clone()),
            NodeValue::SoftBreak => OwnedInline::SoftBreak,
            NodeValue::LineBreak => OwnedInline::HardBreak,
            NodeValue::HtmlInline(_) => OwnedInline::Skip,
            NodeValue::Math(math) => OwnedInline::Code(math.literal.clone()),
            NodeValue::FootnoteReference(note) => OwnedInline::Footnote {
                dest: format!("#fn-{}", slug(&note.name)),
                label: format!("[{}]", note.ix.max(1)),
            },
            NodeValue::Link(link) => OwnedInline::Link(link.url.clone()),
            NodeValue::Image(link) => OwnedInline::Image(link.url.clone()),
            NodeValue::Emph => OwnedInline::Emphasis,
            NodeValue::Strong => OwnedInline::Strong,
            NodeValue::Strikethrough => OwnedInline::Strike,
            _ => OwnedInline::Skip,
        }
    };
    match owned {
        OwnedInline::Text(text) => Some(Inline::Text(text)),
        OwnedInline::Code(code) => Some(Inline::Code(code)),
        OwnedInline::SoftBreak => Some(Inline::SoftBreak),
        OwnedInline::HardBreak => Some(Inline::HardBreak),
        OwnedInline::Footnote { dest, label } => Some(Inline::Link {
            dest,
            children: vec![Inline::Text(label)],
        }),
        OwnedInline::Link(dest) => Some(Inline::Link {
            dest,
            children: inlines_of(node),
        }),
        OwnedInline::Image(dest) => Some(Inline::Image {
            alt: plain(&inlines_of(node)),
            dest,
            width: None,
        }),
        OwnedInline::Emphasis => Some(Inline::Emphasis(inlines_of(node))),
        OwnedInline::Strong => Some(Inline::Strong(inlines_of(node))),
        OwnedInline::Strike => Some(Inline::Strike(inlines_of(node))),
        OwnedInline::Skip => None,
    }
}

/// Block-level HTML. Headings and `align="center"` containers keep their
/// structure, links keep their target, and other tags are dropped. Every
/// produced section uses the lines of the HTML block it came from.
pub fn html_sections(html: &str, start_line: u32, end_line: u32) -> Vec<Section> {
    let mut scan = HtmlScan {
        start_line,
        end_line: end_line.max(start_line),
        ..HtmlScan::default()
    };
    scan.run(html);
    scan.end_block();
    scan.out
}

/// Visible inline text for a single raw HTML tag or a run of inline HTML.
pub fn html_inlines(html: &str) -> Vec<Inline> {
    let mut scan = HtmlScan::default();
    scan.run(html);
    scan.flush_text();
    scan.para
}

#[derive(Default)]
struct HtmlScan {
    out: Vec<Section>,
    para: Vec<Inline>,
    buf: String,
    strong: i32,
    emphasis: i32,
    link: Option<String>,
    heading: Option<u8>,
    /// Open block containers, with whether they centre their content.
    containers: Vec<(String, bool)>,
    start_line: u32,
    end_line: u32,
}

impl HtmlScan {
    fn run(&mut self, html: &str) {
        let mut rest = html;
        while !rest.is_empty() {
            if rest.starts_with('<') {
                self.flush_text();
                if let Some(after) = rest.strip_prefix("<!--") {
                    rest = after.split_once("-->").map_or("", |(_, tail)| tail);
                    continue;
                }
                let Some((tag, tail)) = rest[1..].split_once('>') else {
                    break;
                };
                rest = tail;
                self.tag(tag);
                continue;
            }
            if let Some((ch, len)) = decode_entity(rest) {
                self.buf.push(ch);
                rest = &rest[len..];
                continue;
            }
            let ch = rest.chars().next().unwrap_or(' ');
            self.buf.push(if ch.is_whitespace() { ' ' } else { ch });
            rest = &rest[ch.len_utf8()..];
        }
    }

    fn tag(&mut self, tag: &str) {
        let (name, closing, attrs) = parse_tag(tag);
        match name.as_str() {
            "br" => self.para.push(Inline::HardBreak),
            "img" => {
                if let Some(dest) = attr_value(attrs, "src") {
                    let image = Inline::Image {
                        alt: attr_value(attrs, "alt").unwrap_or_default(),
                        dest,
                        width: image_width(attrs),
                    };
                    self.para.push(match &self.link {
                        Some(link) => Inline::Link {
                            dest: link.clone(),
                            children: vec![image],
                        },
                        None => image,
                    });
                }
            }
            "strong" | "b" => adjust(&mut self.strong, closing),
            "em" | "i" => adjust(&mut self.emphasis, closing),
            "a" => {
                self.link = if closing {
                    None
                } else {
                    attr_value(attrs, "href")
                };
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.end_block();
                self.heading = if closing {
                    None
                } else {
                    name[1..].parse().ok()
                };
            }
            "div" | "p" | "center" | "section" | "details" | "summary" => {
                self.end_block();
                if closing {
                    if let Some(index) = self.containers.iter().rposition(|(open, _)| *open == name)
                    {
                        self.containers.truncate(index);
                    }
                } else {
                    let centered = name == "center" || is_center(attrs);
                    self.containers.push((name, centered));
                }
            }
            _ => {}
        }
    }

    fn centered(&self) -> bool {
        self.containers.iter().any(|(_, centered)| *centered)
    }

    fn flush_text(&mut self) {
        if let Some(inline) = styled_text(
            &mut self.buf,
            self.strong > 0,
            self.emphasis > 0,
            self.link.as_deref(),
        ) {
            self.para.push(inline);
        }
    }

    fn end_block(&mut self) {
        self.flush_text();
        let inlines = std::mem::take(&mut self.para);
        if inlines.is_empty() {
            return;
        }
        let block = match self.heading {
            Some(level) => Block::Heading { level, inlines },
            None => paragraph_or_images(inlines),
        };
        self.emit(block);
    }

    fn emit(&mut self, block: Block) {
        let (start_line, end_line) = (self.start_line, self.end_line);
        let inner = Section {
            start_line,
            end_line,
            block,
        };
        let block = if self.centered() {
            Block::Center(vec![inner])
        } else {
            inner.block
        };
        self.out.push(Section {
            start_line,
            end_line,
            block,
        });
    }
}

fn adjust(depth: &mut i32, closing: bool) {
    *depth = if closing {
        (*depth - 1).max(0)
    } else {
        *depth + 1
    };
}

/// Lowercase tag name, whether it is a closing tag, and the raw attribute text.
fn parse_tag(tag: &str) -> (String, bool, &str) {
    let trimmed = tag.trim();
    let closing = trimmed.starts_with('/');
    let body = trimmed.trim_start_matches('/');
    let name_end = body
        .find(|ch: char| ch.is_whitespace() || ch == '/')
        .unwrap_or(body.len());
    (
        body[..name_end].to_ascii_lowercase(),
        closing,
        &body[name_end..],
    )
}

fn is_center(attrs: &str) -> bool {
    attr_value(attrs, "align").is_some_and(|value| value.eq_ignore_ascii_case("center"))
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let start = lower.find(format!("{name}=").as_str())? + name.len() + 1;
    let rest = attrs.get(start..)?.trim_start();
    let value = match rest.chars().next()? {
        quote @ ('"' | '\'') => rest[1..].split(quote).next()?,
        _ => rest.split_whitespace().next()?,
    };
    Some(value.to_owned())
}

fn image_width(attrs: &str) -> Option<ImageWidth> {
    let value = attr_value(attrs, "width")?;
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse().ok().map(ImageWidth::Percent);
    }
    value
        .trim_end_matches("px")
        .trim()
        .parse()
        .ok()
        .map(ImageWidth::Px)
}

fn styled_text(
    buf: &mut String,
    strong: bool,
    emphasis: bool,
    link: Option<&str>,
) -> Option<Inline> {
    let leading = buf.starts_with(char::is_whitespace);
    let trailing = buf.ends_with(char::is_whitespace);
    let words = buf.split_whitespace().collect::<Vec<_>>().join(" ");
    buf.clear();
    if words.is_empty() {
        return None;
    }
    let mut text = words;
    if leading {
        text.insert(0, ' ');
    }
    if trailing {
        text.push(' ');
    }
    let mut inline = Inline::Text(text);
    if emphasis {
        inline = Inline::Emphasis(vec![inline]);
    }
    if strong {
        inline = Inline::Strong(vec![inline]);
    }
    if let Some(dest) = link {
        inline = Inline::Link {
            dest: dest.to_owned(),
            children: vec![inline],
        };
    }
    Some(inline)
}

fn decode_entity(text: &str) -> Option<(char, usize)> {
    let rest = text.strip_prefix('&')?;
    let end = rest.find(';')?;
    if end == 0 || end > 16 {
        return None;
    }
    let name = &rest[..end];
    let ch = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00a0}',
        _ => {
            if let Some(hex) = name.strip_prefix("#x") {
                char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
            } else {
                let dec = name.strip_prefix('#')?;
                char::from_u32(dec.parse().ok()?)?
            }
        }
    };
    Some((ch, end + 2))
}

fn alert_kind(kind: AlertType) -> AlertKind {
    match kind {
        AlertType::Note => AlertKind::Note,
        AlertType::Tip => AlertKind::Tip,
        AlertType::Important => AlertKind::Important,
        AlertType::Warning => AlertKind::Warning,
        AlertType::Caution => AlertKind::Caution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heading_emphasis_and_link() {
        let doc = parse("# Title\n\nHello **bold** and [docs](https://example.com).\n");
        assert!(matches!(
            &doc.blocks[0].block,
            Block::Heading { level: 1, inlines } if inlines == &vec![Inline::Text("Title".into())]
        ));
        assert_eq!(doc.blocks[0].start_line, 1);
        assert_eq!(doc.blocks[1].start_line, 3);
        match &doc.blocks[1].block {
            Block::Paragraph(inlines) => {
                assert!(matches!(inlines[0], Inline::Text(ref text) if text == "Hello "));
                assert!(matches!(inlines[1], Inline::Strong(_)));
                assert!(matches!(
                    &inlines[3],
                    Inline::Link { dest, .. } if dest == "https://example.com"
                ));
            }
            other => panic!("paragraph, got {other:?}"),
        }
    }

    #[test]
    fn parses_tasks_code_table_image_and_alert() {
        let doc = parse(
            r#"
- [x] done
- [ ] todo

```rust
fn main() {}
```

| a | b |
| - | - |
| 1 | 2 |

![diagram](pic.png)

> [!WARNING]
> Careful
"#,
        );
        let list = doc
            .blocks
            .iter()
            .find_map(|section| match &section.block {
                Block::List { items, .. } => Some(items),
                _ => None,
            })
            .expect("list");
        assert_eq!(list[0].checked, Some(true));
        assert_eq!(list[1].checked, Some(false));

        assert!(doc.blocks.iter().any(|section| matches!(
            &section.block,
            Block::Code { lang, literal } if lang == "rust" && literal.contains("fn main")
        )));
        assert!(doc.blocks.iter().any(|section| matches!(
            &section.block,
            Block::Table { header, rows, .. } if header.len() == 2 && rows.len() == 1
        )));
        assert!(doc.blocks.iter().any(|section| matches!(
            &section.block,
            Block::Images(images) if images[0].alt == "diagram" && images[0].dest == "pic.png"
        )));
        assert!(doc.blocks.iter().any(|section| matches!(
            &section.block,
            Block::Alert {
                kind: AlertKind::Warning,
                ..
            }
        )));
    }

    #[test]
    fn slug_is_lowercase_and_hyphenated() {
        assert_eq!(slug("Hello, World!"), "hello-world");
    }

    #[test]
    fn html_image_width_is_kept() {
        let doc = parse(
            "<img src=\"a.png\" width=\"100%\" alt=\"wide\">\n\n<img src=\"b.png\" width=\"760\">\n",
        );
        let mut widths = Vec::new();
        for section in &doc.blocks {
            if let Block::Images(images) = &section.block {
                widths.extend(images.iter().map(|image| image.width));
            }
        }
        assert_eq!(
            widths,
            vec![
                Some(ImageWidth::Percent(100.0)),
                Some(ImageWidth::Px(760.0))
            ]
        );
    }

    #[test]
    fn html_headings_and_centred_containers_keep_their_structure() {
        let doc = parse(
            "<div align=\"center\">\n  <h1>md</h1>\n  <p><a href=\"#install\">Install</a> and <a href=\"#quickstart\">Quickstart</a></p>\n</div>\n",
        );
        let Block::Center(inner) = &doc.blocks[0].block else {
            panic!("centred heading, got {:?}", doc.blocks[0].block);
        };
        assert!(matches!(
            &inner[0].block,
            Block::Heading { level: 1, inlines } if plain(inlines) == "md"
        ));
        let Block::Center(links) = &doc.blocks[1].block else {
            panic!("centred paragraph, got {:?}", doc.blocks[1].block);
        };
        let Block::Paragraph(inlines) = &links[0].block else {
            panic!("paragraph inside the container");
        };
        assert!(matches!(
            &inlines[0],
            Inline::Link { dest, .. } if dest == "#install"
        ));
    }

    #[test]
    fn a_row_of_linked_badges_is_one_image_row() {
        let doc = parse(
            "<p>\n  <img alt=\"License\" src=\"https://img.shields.io/a.svg\">\n  <a href=\"https://crates.io/crates/md\"><img alt=\"crates.io\" src=\"https://img.shields.io/b.svg\"></a>\n</p>\n\n[![ci](ci.svg)](https://ci.example) ![logo](logo.png)\n",
        );
        let Block::Images(html) = &doc.blocks[0].block else {
            panic!("html badge row, got {:?}", doc.blocks[0].block);
        };
        assert_eq!(html.len(), 2);
        assert_eq!(html[0].alt, "License");
        assert_eq!(html[1].link.as_deref(), Some("https://crates.io/crates/md"));
        let Block::Images(markdown) = &doc.blocks[1].block else {
            panic!("markdown badge row, got {:?}", doc.blocks[1].block);
        };
        assert_eq!(markdown[0].link.as_deref(), Some("https://ci.example"));
        assert_eq!(markdown[1].dest, "logo.png");
    }

    #[test]
    fn details_hold_the_markdown_between_their_tags() {
        let doc = parse(
            "<details>\n<summary><strong>Build</strong> it</summary>\n\n```bash\nmake\n```\n\nMore text.\n\n</details>\n\nAfter.\n",
        );
        let Block::Details {
            summary,
            open,
            blocks,
        } = &doc.blocks[0].block
        else {
            panic!("details, got {:?}", doc.blocks[0].block);
        };
        assert!(!open);
        assert_eq!(plain(summary), "Build it");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0].block, Block::Code { literal, .. } if literal == "make\n"));
        assert!(matches!(&doc.blocks[1].block, Block::Paragraph(_)));
    }

    #[test]
    fn html_wrappers_are_not_shown_as_source() {
        let doc = parse(
            "<details>\n<summary><strong>Build</strong> it</summary>\n\ninside\n\n</details>\n\n<a id=\"quickstart\"></a>\n## Quickstart\n",
        );
        let text = doc
            .blocks
            .iter()
            .map(|section| match &section.block {
                Block::Paragraph(inlines) | Block::Heading { inlines, .. } => plain(inlines),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Build"));
        assert!(text.contains("it"));
        assert!(text.contains("inside"));
        assert!(text.contains("Quickstart"));
        assert!(!text.contains('<'));
        assert!(doc.blocks.iter().any(|section| matches!(
            &section.block,
            Block::Details { summary, .. } if matches!(summary.first(), Some(Inline::Strong(_)))
        )));
    }
}
