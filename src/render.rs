//! Lay out a document into a pixmap and hit targets.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use cosmic_text::{
    Align as TextAlign, Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping,
    Style as FontStyle, SwashCache, Weight,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::document::{
    AlertKind, Align, Block, Document, ImageRef, ImageWidth, Inline, Item, Section, plain, slug,
};
use crate::fasttext::{FastText, LayoutOpts, Spec};
use crate::images::{ImageStore, Lookup, Picture};
use crate::page::{Atom, Frame, Rect, Region, RegionKind};

pub type Rgba = [u8; 4];

const MAX_WIDTH: u32 = 8192;
const MAX_HEIGHT: u32 = 32768;
/// Body text size the layout constants were designed for.
const BASE_TEXT_PX: f32 = 16.0;
/// Reading column width in body-text ems. Keeps lines near 70 characters.
const MEASURE_EM: f32 = 40.0;
/// Proportional face for body text. Falls back to the default font where missing.
const SANS: Family<'static> = Family::Name("Helvetica Neue");

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Rgba,
    pub fg: Rgba,
    pub muted: Rgba,
    pub heading: Rgba,
    pub link: Rgba,
    pub code_bg: Rgba,
    pub inline_bg: Rgba,
    pub code_fg: Rgba,
    pub border: Rgba,
    pub quote: Rgba,
    pub accent: Rgba,
    pub selection: Rgba,
    pub syntax: SyntaxColors,
    pub dark: bool,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: [13, 17, 23, 255],
            fg: [230, 237, 243, 255],
            muted: [139, 148, 158, 255],
            heading: [255, 255, 255, 255],
            link: [88, 166, 255, 255],
            code_bg: [22, 27, 34, 255],
            inline_bg: [36, 41, 48, 255],
            code_fg: [230, 237, 243, 255],
            border: [48, 54, 61, 255],
            quote: [88, 166, 255, 255],
            accent: [63, 185, 80, 255],
            selection: [56, 139, 253, 96],
            syntax: SyntaxColors::fallback(true),
            dark: true,
        }
    }

    pub fn light() -> Self {
        Self {
            bg: [255, 255, 255, 255],
            fg: [31, 35, 40, 255],
            muted: [101, 109, 118, 255],
            heading: [31, 35, 40, 255],
            link: [9, 105, 218, 255],
            code_bg: [246, 248, 250, 255],
            inline_bg: [232, 236, 240, 255],
            code_fg: [31, 35, 40, 255],
            border: [208, 215, 222, 255],
            quote: [9, 105, 218, 255],
            accent: [26, 127, 55, 255],
            selection: [9, 105, 218, 70],
            syntax: SyntaxColors::fallback(false),
            dark: false,
        }
    }

    pub fn choose(
        pref: ThemeChoice,
        background: Option<(u8, u8, u8)>,
        palette: &[Option<(u8, u8, u8)>; 16],
    ) -> Self {
        let light = background.is_some_and(crate::protocol::background_is_light);
        let mut theme = match pref {
            ThemeChoice::Dark => Self::dark(),
            ThemeChoice::Light => Self::light(),
            ThemeChoice::Auto if light => Self::light(),
            ThemeChoice::Auto => Self::dark(),
        };
        if let Some((red, green, blue)) = background.filter(|_| light != theme.dark) {
            theme = theme.on_background((red, green, blue));
        }
        theme.apply_palette(palette);
        theme
    }

    /// Blend into the terminal: use its background and derive surfaces from it.
    fn on_background(mut self, (red, green, blue): (u8, u8, u8)) -> Self {
        self.bg = [red, green, blue, 255];
        self.code_bg = mix(self.bg, self.fg, 0.06);
        self.inline_bg = mix(self.bg, self.fg, 0.10);
        self.border = mix(self.bg, self.fg, 0.22);
        self
    }

    /// Recolor links and code from the terminal's ANSI palette, the way the
    /// page on the right picks up the terminal theme instead of a fixed one.
    fn apply_palette(&mut self, palette: &[Option<(u8, u8, u8)>; 16]) {
        let colors = resolved_palette(palette, self.dark);
        let bg = (self.bg[0], self.bg[1], self.bg[2]);
        let code = (self.code_bg[0], self.code_bg[1], self.code_bg[2]);
        let pick = |index: usize| hue_pick(colors[index], colors[index + 8], code);
        let on_code = |rgb: (u8, u8, u8)| ensure_contrast(rgba(rgb), self.code_bg, 4.5, self.dark);
        self.link = ensure_contrast(
            rgba(hue_pick(colors[4], colors[12], bg)),
            self.bg,
            4.5,
            self.dark,
        );
        self.quote = self.link;
        self.accent = on_code(pick(2));
        self.syntax = SyntaxColors {
            keyword: on_code(pick(5)),
            string: on_code(pick(2)),
            number: on_code(pick(6)),
            comment: on_code(colors[8]),
            function: on_code(pick(4)),
            ty: on_code(pick(3)),
            variable: on_code(pick(1)),
            operator: self.fg,
            punctuation: ensure_contrast(mix(self.fg, self.bg, 0.25), self.code_bg, 4.5, self.dark),
            meta: on_code(pick(1)),
            attribute: on_code(pick(3)),
        };
    }
}

/// Syntax colors taken from the terminal palette: magenta keywords, green
/// strings, yellow types, cyan numbers, blue functions, red variables.
#[derive(Clone, Copy, Debug)]
pub struct SyntaxColors {
    pub keyword: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub comment: Rgba,
    pub function: Rgba,
    pub ty: Rgba,
    pub variable: Rgba,
    pub operator: Rgba,
    pub punctuation: Rgba,
    pub meta: Rgba,
    pub attribute: Rgba,
}

impl SyntaxColors {
    fn fallback(dark: bool) -> Self {
        if !dark {
            return Self {
                keyword: [130, 80, 223, 255],
                string: [26, 127, 55, 255],
                number: [27, 124, 131, 255],
                comment: [87, 96, 106, 255],
                function: [9, 105, 218, 255],
                ty: [99, 60, 1, 255],
                variable: [164, 14, 38, 255],
                operator: [31, 35, 40, 255],
                punctuation: [87, 96, 106, 255],
                meta: [164, 14, 38, 255],
                attribute: [99, 60, 1, 255],
            };
        }
        let mut theme = Theme {
            bg: [30, 30, 30, 255],
            fg: [212, 212, 212, 255],
            muted: [140, 140, 140, 255],
            heading: [212, 212, 212, 255],
            link: [129, 162, 190, 255],
            code_bg: [37, 37, 38, 255],
            inline_bg: [48, 48, 49, 255],
            code_fg: [212, 212, 212, 255],
            border: [68, 68, 68, 255],
            quote: [129, 162, 190, 255],
            accent: [181, 189, 104, 255],
            selection: [0, 0, 0, 0],
            syntax: Self::plain([212, 212, 212, 255]),
            dark: true,
        };
        theme.apply_palette(&[None; 16]);
        theme.syntax
    }

    fn plain(color: Rgba) -> Self {
        Self {
            keyword: color,
            string: color,
            number: color,
            comment: color,
            function: color,
            ty: color,
            variable: color,
            operator: color,
            punctuation: color,
            meta: color,
            attribute: color,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ThemeChoice {
    #[default]
    Auto,
    Dark,
    Light,
}

pub struct Page {
    pub pixmap: Pixmap,
    pub regions: Vec<Region>,
    pub atoms: Vec<Atom>,
    pub frames: Vec<Frame>,
}

impl Page {
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }
}

pub struct StatusBar<'a> {
    pub dir: &'a str,
    pub file: &'a str,
    pub note: &'a str,
    pub hints: &'a str,
    /// Fraction of the page above the bottom of the viewport.
    pub progress: f32,
}

pub struct Renderer {
    fonts: FontSystem,
    cache: SwashCache,
    syntax: SyntaxSet,
    fast: FastText,
    images: ImageStore,
    /// `<details>` blocks, by document order, whose open state the user flipped.
    toggled: HashSet<usize>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
            syntax: SyntaxSet::load_defaults_newlines(),
            fast: FastText::new(),
            images: ImageStore::default(),
            toggled: HashSet::new(),
        }
    }

    /// Flips the open state of the `index`th `<details>` block.
    pub fn toggle_details(&mut self, index: usize) {
        if !self.toggled.remove(&index) {
            self.toggled.insert(index);
        }
    }

    /// Returns every `<details>` block to its default state, for a new document.
    pub fn reset_details(&mut self) {
        self.toggled.clear();
    }

    /// Footer drawn in pixels: a scroll progress line, the file, a note, and key hints.
    pub fn status_bar(
        &mut self,
        bar: &StatusBar,
        width: u32,
        height: u32,
        theme: &Theme,
    ) -> Pixmap {
        let (width, height) = (width.max(1), height.max(1));
        let mut pixmap = Pixmap::new(width, height).expect("status pixmap");
        pixmap.fill(skia_color(theme.code_bg));
        let rule = (height as f32 * 0.07).round().max(2.0);
        fill_rect(&mut pixmap, 0.0, 0.0, width as f32, rule, theme.border);
        let filled = width as f32 * bar.progress.clamp(0.0, 1.0);
        fill_rect(&mut pixmap, 0.0, 0.0, filled, rule, theme.link);

        let size = (height as f32 * 0.4).max(9.0);
        let pad = size * 1.1;
        let mid = (height as f32 + rule) / 2.0;
        let separator = if bar.dir.is_empty() { "" } else { " › " };
        let note = if bar.note.is_empty() {
            String::new()
        } else {
            format!("   {}", bar.note)
        };
        let left = [
            (bar.dir, theme.muted, Weight::NORMAL),
            (separator, theme.muted, Weight::NORMAL),
            (bar.file, theme.fg, Weight::SEMIBOLD),
            (note.as_str(), theme.link, Weight::NORMAL),
        ];
        let (left, _) = self.shape_line(&left, size);
        let (right, right_w) = self.shape_line(&[(bar.hints, theme.muted, Weight::NORMAL)], size);
        self.draw_line(&mut pixmap, &right, width as f32 - pad - right_w, mid, size);
        self.draw_line(&mut pixmap, &left, pad, mid, size);
        pixmap
    }

    fn shape_line(&mut self, spans: &[(&str, Rgba, Weight)], size: f32) -> (Buffer, f32) {
        let metrics = Metrics::new(size, size * 1.3);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        buffer.set_size(&mut self.fonts, None, None);
        let rich: Vec<(&str, Attrs)> = spans
            .iter()
            .filter(|(text, _, _)| !text.is_empty())
            .map(|(text, color, weight)| {
                (
                    *text,
                    Attrs::new()
                        .family(SANS)
                        .weight(*weight)
                        .color(rgba_text(*color))
                        .metrics(metrics),
                )
            })
            .collect();
        let default = Attrs::new().metrics(metrics);
        buffer.set_rich_text(&mut self.fonts, rich, &default, Shaping::Advanced, None);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max);
        (buffer, width)
    }

    fn draw_line(&mut self, pixmap: &mut Pixmap, buffer: &Buffer, x: f32, mid: f32, size: f32) {
        let top = (mid - size * 1.3 / 2.0).round() as i32;
        let left = x.round() as i32;
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            TextColor::rgb(255, 255, 255),
            |gx, gy, w, h, color| {
                fill_rect(
                    pixmap,
                    (left + gx) as f32,
                    (top + gy) as f32,
                    w as f32,
                    h as f32,
                    color.as_rgba(),
                );
            },
        );
    }

    /// True when a remote image arrived since the last call; render again to show it.
    pub fn take_image_updates(&self) -> bool {
        self.images.take_updated()
    }

    /// True while a badge or other remote image is still downloading.
    pub fn images_pending(&self) -> bool {
        self.images.pending()
    }

    /// `text_px` is the body text size at zoom 1. Pass the terminal's text
    /// size so the page reads at the same scale as the terminal.
    pub fn render(
        &mut self,
        doc: &Document,
        viewport_width: f32,
        theme: &Theme,
        zoom: f32,
        text_px: f32,
        base_dir: &Path,
    ) -> Page {
        warm_images(&self.images, &doc.blocks);
        let mut lay = Layouter::new(self, theme, zoom, text_px, viewport_width, base_dir);
        lay.render_blocks(&doc.blocks, lay.left, lay.content_width);
        lay.finish()
    }
}

/// Start remote image loads before text layout, so a cached badge is ready for
/// the first frame and a missing one is already on the network.
fn warm_images(images: &ImageStore, blocks: &[Section]) {
    for section in blocks {
        match &section.block {
            Block::Images(list) => {
                for image in list {
                    if image.dest.starts_with("http://") || image.dest.starts_with("https://") {
                        let _ = images.get(&image.dest, Path::new(""));
                    }
                }
            }
            Block::Center(inner)
            | Block::Quote(inner)
            | Block::Details { blocks: inner, .. }
            | Block::Footnote { blocks: inner, .. }
            | Block::Alert { blocks: inner, .. } => warm_images(images, inner),
            Block::List { items, .. } => {
                for item in items {
                    warm_images(images, &item.blocks);
                }
            }
            _ => {}
        }
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct SpanStyle {
    weight: Weight,
    font_style: FontStyle,
    mono: bool,
    color: Rgba,
    link: Option<String>,
    strike: bool,
    code: bool,
    size: f32,
}

impl SpanStyle {
    fn same(&self, other: &Self) -> bool {
        self.weight == other.weight
            && self.font_style == other.font_style
            && self.mono == other.mono
            && self.color == other.color
            && self.link == other.link
            && self.strike == other.strike
            && self.code == other.code
            && (self.size - other.size).abs() < 0.1
    }
}

struct SpanInfo {
    range: Range<usize>,
    link: Option<String>,
    strike: bool,
    code: bool,
}

struct Piece {
    text: String,
    attrs: Attrs<'static>,
    info: SpanInfo,
}

enum Op {
    Fill {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Rgba,
    },
    /// Triangle for a `<details>` summary: right when closed, down when open.
    Disclosure {
        x: f32,
        y: f32,
        size: f32,
        open: bool,
        color: Rgba,
    },
    Round {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: Rgba,
    },
    Image {
        x: f32,
        y: f32,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    Text(TextOp),
    Glyphs(Vec<crate::fasttext::Ink>),
}

struct RowItem {
    w: f32,
    h: f32,
    target: String,
    kind: RowKind,
}

enum RowKind {
    Picture(std::sync::Arc<Picture>),
    Label(String),
}

struct TextOp {
    buffer: Buffer,
    x: f32,
    y: f32,
    color: TextColor,
    full: String,
    spans: Vec<SpanInfo>,
}

struct Layouter<'a> {
    fonts: &'a mut FontSystem,
    fast: &'a mut FastText,
    cache: &'a mut SwashCache,
    syntax: &'a SyntaxSet,
    images: &'a ImageStore,
    toggled: &'a HashSet<usize>,
    details_seq: usize,
    theme: &'a Theme,
    zoom: f32,
    pad: f32,
    left: f32,
    content_width: f32,
    viewport: f32,
    /// True while laying out a centred HTML container.
    center: bool,
    body: f32,
    line: f32,
    gap: f32,
    y: f32,
    max_x: f32,
    base_dir: &'a Path,
    ops: Vec<Op>,
    regions: Vec<Region>,
    atoms: Vec<Atom>,
    frames: Vec<Frame>,
    line_seq: u32,
    slugs: HashMap<String, usize>,
}

impl<'a> Layouter<'a> {
    fn new(
        renderer: &'a mut Renderer,
        theme: &'a Theme,
        zoom: f32,
        text_px: f32,
        viewport: f32,
        base_dir: &'a Path,
    ) -> Self {
        // Every size in the layout is a multiple of `zoom`, so folding the
        // terminal text size into it scales the whole page.
        let zoom = zoom.clamp(0.5, 3.0) * (text_px.max(1.0) / BASE_TEXT_PX);
        let pad = 20.0 * zoom;
        let viewport = viewport.max(pad * 2.0 + 40.0);
        let body = 16.0 * zoom;
        let inner = viewport - pad * 2.0;
        let content_width = (body * MEASURE_EM).min(inner);
        let left = pad + (inner - content_width) / 2.0;
        Self {
            fonts: &mut renderer.fonts,
            fast: &mut renderer.fast,
            cache: &mut renderer.cache,
            syntax: &renderer.syntax,
            images: &renderer.images,
            toggled: &renderer.toggled,
            details_seq: 0,
            theme,
            zoom,
            pad,
            left,
            content_width,
            viewport,
            center: false,
            body,
            line: body * 1.45,
            gap: 14.0 * zoom,
            y: pad,
            max_x: pad,
            base_dir,
            ops: Vec::new(),
            regions: Vec::new(),
            atoms: Vec::new(),
            frames: Vec::new(),
            line_seq: 0,
            slugs: HashMap::new(),
        }
    }

    fn render_blocks(&mut self, sections: &[Section], x: f32, width: f32) {
        for (index, section) in sections.iter().enumerate() {
            if index > 0 {
                self.y += self.gap;
            }
            self.render_section(section, x, width);
        }
    }

    fn render_section(&mut self, section: &Section, x: f32, width: f32) {
        let y0 = self.y;
        self.render_block(&section.block, x, width);
        let height = (self.y - y0).max(1.0);
        self.frames.push(Frame {
            start_line: section.start_line,
            end_line: section.end_line,
            rect: Rect {
                x,
                y: y0,
                w: width,
                h: height,
            },
        });
    }

    fn render_block(&mut self, block: &Block, x: f32, width: f32) {
        match block {
            Block::Heading { level, inlines } => self.render_heading(*level, inlines, x, width),
            Block::Paragraph(inlines) => {
                self.draw_inlines(inlines, x, width, self.body_style());
            }
            Block::Center(sections) => {
                let outer = self.center;
                self.center = true;
                self.render_blocks(sections, x, width);
                self.center = outer;
            }
            Block::Details {
                summary,
                open,
                blocks,
            } => self.render_details(summary, *open, blocks, x, width),
            Block::Code { lang, literal } => self.render_code(lang, literal, x, width),
            Block::Quote(blocks) => self.render_quote(blocks, x, width, self.theme.quote),
            Block::Alert {
                kind,
                title,
                blocks,
            } => self.render_alert(*kind, title, blocks, x, width),
            Block::List {
                ordered,
                start,
                items,
            } => {
                self.render_list(*ordered, *start, items, x, width);
            }
            Block::Table {
                aligns,
                header,
                rows,
            } => {
                self.render_table(aligns, header, rows, x, width);
            }
            Block::Rule => self.render_rule(x, width),
            Block::Images(images) => self.render_images(images, x, width),
            Block::Footnote { name, blocks } => self.render_footnote(name, blocks, x, width),
            Block::FrontMatter(html) => self.render_code("", html, x, width),
        }
    }

    fn render_details(
        &mut self,
        summary: &[Inline],
        open: bool,
        blocks: &[Section],
        x: f32,
        width: f32,
    ) {
        let index = self.details_seq;
        self.details_seq += 1;
        let open = open != self.toggled.contains(&index);
        let y0 = self.y;
        let size = self.body * 0.6;
        self.ops.push(Op::Disclosure {
            x,
            y: y0 + (self.line - size) / 2.0,
            size,
            open,
            color: self.theme.fg,
        });
        let indent = size + self.body * 0.5;
        self.draw_inlines(summary, x + indent, width - indent, self.body_style());
        self.regions.push(Region {
            rect: Rect {
                x,
                y: y0,
                w: width,
                h: (self.y - y0).max(self.line),
            },
            kind: RegionKind::Toggle(index),
        });
        self.mark(x + indent);
        if open && !blocks.is_empty() {
            self.y += self.gap;
            self.render_blocks(blocks, x, width);
        }
    }

    fn render_heading(&mut self, level: u8, inlines: &[Inline], x: f32, width: f32) {
        let scale = match level {
            1 => 2.0,
            2 => 1.6,
            3 => 1.35,
            4 => 1.15,
            _ => 1.05,
        };
        if level <= 3 && self.y > self.pad + 1.0 {
            self.y += 12.0 * self.zoom;
        }
        let y0 = self.y;
        let mut style = self.body_style();
        style.size = self.body * scale;
        style.color = self.theme.heading;
        style.weight = Weight::BOLD;
        self.draw_inlines(inlines, x, width, style);
        if level <= 2 {
            self.y += if level == 1 { 10.0 } else { 6.0 } * self.zoom;
            self.ops.push(Op::Fill {
                x,
                y: self.y,
                w: width,
                h: self.zoom.max(1.0),
                color: self.theme.border,
            });
            self.y += 4.0 * self.zoom;
        }
        let name = self.unique_slug(&plain(inlines));
        self.regions.push(Region {
            rect: Rect {
                x,
                y: y0,
                w: width,
                h: (self.y - y0).max(self.line),
            },
            kind: RegionKind::Anchor(name),
        });
    }

    fn render_code(&mut self, lang: &str, literal: &str, x: f32, width: f32) {
        let pad = 12.0 * self.zoom;
        let y0 = self.y;
        let lines = self.highlight(lang, literal);
        // One buffer for the whole block. Shaping each line on its own is what
        // made a long README take a third of a second, and a resize did it again.
        let mut pieces = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                pieces.push(("\n".to_owned(), self.code_style(self.theme.code_fg)));
            }
            pieces.extend(line.iter().cloned());
        }
        let ops_before = self.ops.len();
        self.y += pad;
        let (text_w, text_h) = self.layout_pieces(&pieces, x + pad, self.y, None);
        let line_h = self.body * 0.92 * 1.45;
        let min_h = line_h * lines.len().max(1) as f32;
        self.y += if text_h > 1.0 { text_h } else { min_h };
        self.y += pad;
        let max_w = text_w;
        let block_w = (max_w + pad * 2.0).max(width);
        self.ops.insert(
            ops_before,
            Op::Round {
                x,
                y: y0,
                w: block_w,
                h: self.y - y0,
                radius: 6.0 * self.zoom,
                color: self.theme.code_bg,
            },
        );
        self.mark(x + block_w);
    }

    fn render_quote(&mut self, blocks: &[Section], x: f32, width: f32, color: Rgba) {
        let y0 = self.y;
        let inset = 16.0 * self.zoom;
        self.render_blocks(blocks, x + inset, (width - inset).max(40.0));
        let height = (self.y - y0).max(self.line);
        self.ops.push(Op::Fill {
            x,
            y: y0,
            w: 3.0 * self.zoom,
            h: height,
            color,
        });
    }

    fn render_alert(
        &mut self,
        kind: AlertKind,
        title: &str,
        blocks: &[Section],
        x: f32,
        width: f32,
    ) {
        let color = alert_color(kind);
        let y0 = self.y;
        let mut style = self.body_style();
        style.weight = Weight::BOLD;
        style.color = color;
        self.draw_inlines(
            &[Inline::Text(title.to_owned())],
            x + 16.0 * self.zoom,
            width,
            style,
        );
        self.y += self.gap * 0.4;
        self.render_blocks(
            blocks,
            x + 16.0 * self.zoom,
            (width - 16.0 * self.zoom).max(40.0),
        );
        self.ops.push(Op::Fill {
            x,
            y: y0,
            w: 3.0 * self.zoom,
            h: (self.y - y0).max(self.line),
            color,
        });
    }

    fn render_list(&mut self, ordered: bool, start: usize, items: &[Item], x: f32, width: f32) {
        let indent = 26.0 * self.zoom;
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.y += self.gap * 0.45;
            }
            let y0 = self.y;
            if let Some(checked) = item.checked {
                self.checkbox(x, y0, checked);
            } else {
                let marker = if ordered {
                    format!("{}.", start + index)
                } else {
                    "•".to_owned()
                };
                let mut style = self.body_style();
                style.color = self.theme.muted;
                self.draw_inlines(&[Inline::Text(marker)], x, indent, style);
                self.y = y0;
            }
            self.render_blocks(
                item.blocks.as_slice(),
                x + indent,
                (width - indent).max(40.0),
            );
        }
    }

    fn checkbox(&mut self, x: f32, y: f32, checked: bool) {
        let size = self.body * 0.85;
        let top = y + (self.line - size) * 0.35;
        self.ops.push(Op::Fill {
            x,
            y: top,
            w: size,
            h: size,
            color: self.theme.border,
        });
        let inset = 1.5 * self.zoom;
        self.ops.push(Op::Fill {
            x: x + inset,
            y: top + inset,
            w: size - inset * 2.0,
            h: size - inset * 2.0,
            color: if checked {
                self.theme.accent
            } else {
                self.theme.bg
            },
        });
    }

    fn render_table(
        &mut self,
        aligns: &[Align],
        header: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        x: f32,
        width: f32,
    ) {
        let cols = header.len().max(rows.first().map_or(1, Vec::len)).max(1);
        let mut grid: Vec<&[Vec<Inline>]> = Vec::with_capacity(1 + rows.len());
        grid.push(header);
        for row in rows {
            grid.push(row.as_slice());
        }
        let pad_x = 12.0 * self.zoom;
        let pad_y = 7.0 * self.zoom;
        let floor = pad_x * 2.0 + 8.0 * self.zoom;
        let mut min_w = vec![floor; cols];
        let mut max_w = vec![floor; cols];
        for (index, row) in grid.iter().enumerate() {
            let style = self.table_style(index == 0);
            for (col, cell) in row.iter().enumerate().take(cols) {
                let (natural, _) = self.measure_inlines(cell, None, &style);
                let word = longest_word(cell);
                let (word_w, _) = if word.is_empty() {
                    (0.0, 0.0)
                } else {
                    self.measure_inlines(&[Inline::Text(word)], None, &style)
                };
                let min = word_w.max(8.0 * self.zoom) + pad_x * 2.0;
                let max = natural.max(word_w).max(8.0 * self.zoom) + pad_x * 2.0;
                min_w[col] = min_w[col].max(min);
                max_w[col] = max_w[col].max(max.max(min_w[col]));
            }
        }
        let widths = column_widths(&min_w, &max_w, width);
        let table_w: f32 = widths.iter().sum();
        let origin = if self.center {
            x + ((width - table_w) / 2.0).max(0.0)
        } else {
            x
        };
        let mut stops = Vec::with_capacity(cols + 1);
        stops.push(origin);
        for column in &widths {
            let edge = stops.last().copied().unwrap_or(origin) + column;
            stops.push(edge);
        }

        let ops_before = self.ops.len();
        let top = self.y;
        let mut bands = Vec::with_capacity(grid.len());
        for (index, row) in grid.iter().enumerate() {
            let header_row = index == 0;
            let style = self.table_style(header_row);
            let y0 = self.y;
            let mut text_w = vec![0.0; cols];
            let mut text_h = vec![self.line; cols];
            for col in 0..cols {
                let Some(cell) = row.get(col) else {
                    continue;
                };
                let inner = (widths[col] - pad_x * 2.0).max(1.0);
                let (measured_w, measured_h) = self.measure_inlines(cell, Some(inner), &style);
                text_w[col] = measured_w;
                text_h[col] = measured_h.max(self.line * 0.6);
            }
            let row_text = text_h.iter().copied().fold(self.line, f32::max);
            let row_h = row_text + pad_y * 2.0;
            for col in 0..cols {
                let Some(cell) = row.get(col) else {
                    continue;
                };
                if cell.is_empty() {
                    continue;
                }
                let inner = (widths[col] - pad_x * 2.0).max(1.0);
                let align = aligns.get(col).copied().unwrap_or(Align::None);
                let shift = match align {
                    Align::Center => ((inner - text_w[col]) / 2.0).max(0.0),
                    Align::Right => (inner - text_w[col]).max(0.0),
                    Align::Left | Align::None => 0.0,
                };
                let rise = ((row_h - pad_y * 2.0 - text_h[col]) / 2.0).max(0.0);
                let pieces = flatten(cell, &style, self.theme.link, self.theme.code_fg);
                self.layout_pieces(
                    &pieces,
                    stops[col] + pad_x + shift,
                    y0 + pad_y + rise,
                    Some(inner),
                );
            }
            self.y = y0 + row_h;
            bands.push((y0, row_h, header_row));
        }
        let bottom = self.y;
        let rule = self.zoom.max(1.0);
        let mut chrome = Vec::new();
        for (index, (y0, row_h, header_row)) in bands.iter().enumerate() {
            let color = if *header_row {
                Some(table_header_bg(self.theme))
            } else if index % 2 == 0 {
                Some(table_stripe(self.theme))
            } else {
                None
            };
            if let Some(color) = color {
                chrome.push(Op::Fill {
                    x: origin,
                    y: *y0,
                    w: table_w,
                    h: *row_h,
                    color,
                });
            }
        }
        for (y0, _, _) in &bands {
            chrome.push(Op::Fill {
                x: origin,
                y: *y0,
                w: table_w,
                h: rule,
                color: self.theme.border,
            });
        }
        chrome.push(Op::Fill {
            x: origin,
            y: (bottom - rule).max(top),
            w: table_w,
            h: rule,
            color: self.theme.border,
        });
        for (index, stop) in stops.iter().enumerate() {
            let line_x = if index + 1 == stops.len() {
                (stop - rule).max(origin)
            } else {
                *stop
            };
            chrome.push(Op::Fill {
                x: line_x,
                y: top,
                w: rule,
                h: (bottom - top).max(rule),
                color: self.theme.border,
            });
        }
        self.ops.splice(ops_before..ops_before, chrome);
        self.mark(origin + table_w);
    }

    fn table_style(&self, header: bool) -> SpanStyle {
        let mut style = self.body_style();
        if header {
            style.weight = Weight::BOLD;
            style.color = self.theme.heading;
        }
        style
    }

    fn render_rule(&mut self, x: f32, width: f32) {
        let y = self.y + self.gap * 0.35;
        let h = self.zoom.max(1.0);
        self.ops.push(Op::Fill {
            x,
            y,
            w: width,
            h,
            color: self.theme.border,
        });
        self.y = y + h + self.gap * 0.35;
        self.mark(x + width);
    }

    fn render_images(&mut self, images: &[ImageRef], x: f32, width: f32) {
        let gap = 6.0 * self.zoom;
        let max_h = 1400.0 * self.zoom;
        let mut label_style = self.body_style();
        label_style.color = self.theme.link;
        let mut items = Vec::new();
        for image in images {
            let target = image.link.clone().unwrap_or_else(|| image.dest.clone());
            match self.images.get(&image.dest, self.base_dir) {
                Lookup::Ready(picture) => {
                    let mut w = match image.width {
                        Some(ImageWidth::Percent(percent)) => width * (percent / 100.0),
                        Some(ImageWidth::Px(px)) => px * self.zoom,
                        None => picture.width * self.zoom,
                    };
                    let mut h = picture.height * (w / picture.width.max(1.0));
                    if w > width {
                        h *= width / w;
                        w = width;
                    }
                    if h > max_h {
                        w *= max_h / h;
                        h = max_h;
                    }
                    items.push(RowItem {
                        w: w.max(1.0),
                        h: h.max(1.0),
                        target,
                        kind: RowKind::Picture(picture),
                    });
                }
                Lookup::Pending | Lookup::Missing => {
                    let label = if image.alt.is_empty() {
                        image.dest.clone()
                    } else {
                        image.alt.clone()
                    };
                    let (w, h) = self.measure_inlines(
                        &[Inline::Text(label.clone())],
                        Some(width),
                        &label_style,
                    );
                    items.push(RowItem {
                        w: w.min(width),
                        h,
                        target,
                        kind: RowKind::Label(label),
                    });
                }
            }
        }

        let mut start = 0;
        while start < items.len() {
            let mut end = start;
            let mut line_w = 0.0_f32;
            while end < items.len() {
                let add = items[end].w + if end > start { gap } else { 0.0 };
                if end > start && line_w + add > width {
                    break;
                }
                line_w += add;
                end += 1;
            }
            let line_h = items[start..end]
                .iter()
                .map(|item| item.h)
                .fold(0.0_f32, f32::max);
            let mut cx = if self.center {
                x + ((width - line_w) / 2.0).max(0.0)
            } else {
                x
            };
            for item in &items[start..end] {
                let iy = self.y + (line_h - item.h) / 2.0;
                match &item.kind {
                    RowKind::Picture(picture) => self.place_picture(picture, cx, iy, item),
                    RowKind::Label(label) => {
                        let pieces = flatten(
                            &[Inline::Text(label.clone())],
                            &label_style,
                            self.theme.link,
                            self.theme.code_fg,
                        );
                        let centered = std::mem::replace(&mut self.center, false);
                        self.layout_pieces(&pieces, cx, iy, None);
                        self.center = centered;
                    }
                }
                self.regions.push(Region {
                    rect: Rect {
                        x: cx,
                        y: iy,
                        w: item.w,
                        h: item.h,
                    },
                    kind: RegionKind::Link(item.target.clone()),
                });
                self.mark(cx + item.w);
                cx += item.w + gap;
            }
            self.y += line_h;
            start = end;
            if start < items.len() {
                self.y += gap;
            }
        }
    }

    fn place_picture(&mut self, picture: &Picture, x: f32, y: f32, item: &RowItem) {
        let w = item.w.round().max(1.0) as u32;
        let h = item.h.round().max(1.0) as u32;
        // Screenshots are far larger than the column. The box filter in
        // `thumbnail` downscales them much faster than a triangle filter.
        let resized = if w < picture.pixels.width() && h < picture.pixels.height() {
            image::imageops::thumbnail(&picture.pixels, w, h)
        } else {
            image::imageops::resize(&picture.pixels, w, h, image::imageops::FilterType::Triangle)
        };
        self.ops.push(Op::Image {
            x,
            y,
            w,
            h,
            pixels: resized.into_raw(),
        });
    }

    fn render_footnote(&mut self, name: &str, blocks: &[Section], x: f32, width: f32) {
        let anchor = format!("fn-{}", slug(name));
        let y0 = self.y;
        let mut style = self.body_style();
        style.color = self.theme.muted;
        style.size = self.body * 0.85;
        self.draw_inlines(&[Inline::Text(format!("[{name}]"))], x, width, style);
        self.regions.push(Region {
            rect: Rect {
                x,
                y: y0,
                w: width,
                h: (self.y - y0).max(self.line),
            },
            kind: RegionKind::Anchor(anchor),
        });
        self.y += self.gap * 0.35;
        self.render_blocks(blocks, x, width);
    }

    fn body_style(&self) -> SpanStyle {
        SpanStyle {
            weight: Weight::NORMAL,
            font_style: FontStyle::Normal,
            mono: false,
            color: self.theme.fg,
            link: None,
            strike: false,
            code: false,
            size: self.body,
        }
    }

    fn draw_inlines(&mut self, inlines: &[Inline], x: f32, width: f32, style: SpanStyle) {
        self.draw_inlines_at(inlines, x, width, style);
    }

    fn draw_inlines_at(&mut self, inlines: &[Inline], x: f32, width: f32, style: SpanStyle) {
        let pieces = flatten(inlines, &style, self.theme.link, self.theme.code_fg);
        let (_, height) = self.layout_pieces(&pieces, x, self.y, Some(width.max(20.0)));
        self.y += height.max(if pieces.is_empty() {
            0.0
        } else {
            self.line * 0.4
        });
    }

    fn measure_inlines(
        &mut self,
        inlines: &[Inline],
        wrap: Option<f32>,
        style: &SpanStyle,
    ) -> (f32, f32) {
        let pieces = flatten(inlines, style, self.theme.link, self.theme.code_fg);
        if pieces.is_empty() {
            return (0.0, self.line);
        }
        let wrap = wrap.map(|value| value.max(1.0));
        if let Some((text_w, text_h)) = self.try_fast(&pieces, wrap, false, 0.0, 0.0, false) {
            return (text_w, text_h);
        }
        let metrics = Metrics::new(style.size.max(1.0), (style.size * 1.45).max(1.0));
        let mut buffer = Buffer::new(self.fonts, metrics);
        buffer.set_size(self.fonts, wrap, None);
        let built = build_pieces(&pieces, metrics);
        let pairs: Vec<(&str, Attrs)> = built
            .iter()
            .map(|piece| (piece.text.as_str(), piece.attrs.clone()))
            .collect();
        let default = Attrs::new().metrics(metrics);
        buffer.set_rich_text(self.fonts, pairs, &default, Shaping::Advanced, None);
        let mut text_w = 0.0_f32;
        let mut text_h = 0.0_f32;
        for run in buffer.layout_runs() {
            text_w = text_w.max(run.line_w);
            text_h = text_h.max(run.line_top + run.line_height);
        }
        (text_w, text_h.max(metrics.line_height))
    }

    /// Places text without a shaper when every character is in the bundled faces.
    /// `emit` records hit targets and paint ops; measuring passes `false`.
    fn try_fast(
        &mut self,
        pieces: &[(String, SpanStyle)],
        wrap: Option<f32>,
        center: bool,
        x: f32,
        y: f32,
        emit: bool,
    ) -> Option<(f32, f32)> {
        let specs: Vec<Spec<'_>> = pieces
            .iter()
            .map(|(text, style)| Spec {
                text,
                mono: style.mono,
                bold: style.weight >= Weight::SEMIBOLD,
                italic: matches!(style.font_style, FontStyle::Italic | FontStyle::Oblique),
                size: style.size,
                color: style.color,
                link: style.link.as_deref(),
                strike: style.strike,
                code: style.code,
            })
            .collect();
        let laid = self
            .fast
            .layout(&specs, LayoutOpts { wrap, center, x, y })?;
        if emit {
            for pill in laid.pills {
                let pad = pill.h * 0.2;
                let inset = pill.h * 0.1;
                self.ops.push(Op::Round {
                    x: pill.x - pad,
                    y: pill.y + inset,
                    w: pill.w + pad * 2.0,
                    h: (pill.h - inset * 2.0).max(1.0),
                    radius: pill.h * 0.22,
                    color: self.theme.inline_bg,
                });
            }
            for strike in laid.strikes {
                self.ops.push(Op::Fill {
                    x: strike.x,
                    y: strike.y,
                    w: strike.w,
                    h: strike.h.max(1.0),
                    color: self.theme.muted,
                });
            }
            for (url, rect) in laid.links {
                self.regions.push(Region {
                    rect,
                    kind: RegionKind::Link(url),
                });
            }
            for (rect, text) in laid.atoms {
                let line = self.line_seq;
                self.line_seq += 1;
                self.atoms.push(Atom { rect, text, line });
            }
            self.ops.push(Op::Glyphs(laid.inks));
            self.mark(x + laid.width);
        }
        Some((laid.width, laid.height))
    }

    fn layout_pieces(
        &mut self,
        pieces: &[(String, SpanStyle)],
        x: f32,
        y: f32,
        wrap: Option<f32>,
    ) -> (f32, f32) {
        if pieces.is_empty() || pieces.iter().all(|(text, _)| text.is_empty()) {
            return (0.0, 0.0);
        }
        if let Some(size) = self.try_fast(pieces, wrap, self.center, x, y, true) {
            return size;
        }
        let size = pieces.first().map_or(self.body, |(_, style)| style.size);
        let metrics = Metrics::new(size.max(1.0), (size * 1.45).max(1.0));
        let mut buffer = Buffer::new(self.fonts, metrics);
        buffer.set_size(self.fonts, wrap, None);
        let built = build_pieces(pieces, metrics);
        let pairs: Vec<(&str, Attrs)> = built
            .iter()
            .map(|piece| (piece.text.as_str(), piece.attrs.clone()))
            .collect();
        let default = Attrs::new().metrics(metrics);
        let shaping = if pieces.iter().all(|(_, style)| style.mono) {
            Shaping::Basic
        } else {
            Shaping::Advanced
        };
        buffer.set_rich_text(self.fonts, pairs, &default, shaping, None);
        if self.center {
            for line in &mut buffer.lines {
                line.set_align(Some(TextAlign::Center));
            }
            buffer.shape_until_scroll(self.fonts, false);
        }
        let mut text_w = 0.0_f32;
        let mut text_h = 0.0_f32;
        for run in buffer.layout_runs() {
            text_w = text_w.max(run.line_w);
            text_h = text_h.max(run.line_top + run.line_height);
        }
        let full = built
            .iter()
            .map(|piece| piece.text.as_str())
            .collect::<String>();
        let spans = built.into_iter().map(|piece| piece.info).collect();
        self.ops.push(Op::Text(TextOp {
            buffer,
            x,
            y,
            color: rgba_text(pieces[0].1.color),
            full,
            spans,
        }));
        self.mark(x + text_w);
        (text_w, text_h.max(metrics.line_height * 0.6))
    }

    fn highlight(&self, lang: &str, literal: &str) -> Vec<Vec<(String, SpanStyle)>> {
        let syntax = self
            .syntax
            .find_syntax_by_token(lang)
            .or_else(|| self.syntax.find_syntax_by_extension(lang))
            .unwrap_or_else(|| self.syntax.find_syntax_plain_text());
        let mut state = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut lines = Vec::new();
        for line in source_lines(literal) {
            // Keep the newline: line-scoped syntax, such as shell comments,
            // only closes when the parser sees it.
            let owned = format!("{line}\n");
            let Ok(ops) = state.parse_line(&owned, self.syntax) else {
                lines.push(vec![(line.to_owned(), self.code_style(self.theme.code_fg))]);
                continue;
            };
            let end = line.len();
            let mut pieces = Vec::new();
            let mut cursor = 0;
            for (pos, op) in &ops {
                let pos = (*pos).min(end);
                if pos > cursor {
                    self.push_token(&mut pieces, &line[cursor..pos], &stack);
                    cursor = pos;
                }
                let _ = stack.apply(op);
            }
            if cursor < end || pieces.is_empty() {
                self.push_token(&mut pieces, &line[cursor..end], &stack);
            }
            lines.push(pieces);
        }
        lines
    }

    fn push_token(&self, pieces: &mut Vec<(String, SpanStyle)>, text: &str, stack: &ScopeStack) {
        if text.is_empty() && !pieces.is_empty() {
            return;
        }
        let color = token_color(&stack.scopes, &self.theme.syntax).unwrap_or(self.theme.code_fg);
        let style = self.code_style(color);
        if let Some((prev, prev_style)) = pieces.last_mut()
            && prev_style.color == style.color
        {
            prev.push_str(text);
            return;
        }
        pieces.push((text.to_owned(), style));
    }

    fn code_style(&self, color: Rgba) -> SpanStyle {
        SpanStyle {
            weight: Weight::NORMAL,
            font_style: FontStyle::Normal,
            mono: true,
            color,
            link: None,
            strike: false,
            code: false,
            size: self.body * 0.92,
        }
    }

    fn unique_slug(&mut self, text: &str) -> String {
        let base = slug(text);
        let base = if base.is_empty() {
            "section".to_owned()
        } else {
            base
        };
        let seen = self.slugs.entry(base.clone()).or_insert(0);
        let name = if *seen == 0 {
            base
        } else {
            format!("{base}-{seen}")
        };
        *seen += 1;
        name
    }

    fn mark(&mut self, right: f32) {
        self.max_x = self.max_x.max(right);
    }

    fn finish(mut self) -> Page {
        let width = (self.max_x + self.pad)
            .ceil()
            .clamp(self.viewport, MAX_WIDTH as f32) as u32;
        let height = (self.y + self.pad).ceil().clamp(1.0, MAX_HEIGHT as f32) as u32;
        let width = width.max(1);
        let mut pixmap = Pixmap::new(width, height)
            .unwrap_or_else(|| Pixmap::new(width.min(1024), height.min(1024)).expect("pixmap"));
        pixmap.fill(skia_color(self.theme.bg));
        let ops = std::mem::take(&mut self.ops);
        for op in ops {
            match op {
                Op::Fill { x, y, w, h, color } => {
                    fill_rect(&mut pixmap, x, y, w, h, color);
                }
                Op::Round {
                    x,
                    y,
                    w,
                    h,
                    radius,
                    color,
                } => fill_round_rect(&mut pixmap, x, y, w, h, radius, color),
                Op::Disclosure {
                    x,
                    y,
                    size,
                    open,
                    color,
                } => fill_disclosure(&mut pixmap, x, y, size, open, color),
                Op::Image { x, y, w, h, pixels } => {
                    blit(
                        &mut pixmap,
                        x.round() as i32,
                        y.round() as i32,
                        w,
                        h,
                        &pixels,
                    );
                }
                Op::Text(text) => self.paint_text(&mut pixmap, text),
                Op::Glyphs(inks) => paint_glyphs(&mut pixmap, self.fast, &inks),
            }
        }
        Page {
            pixmap,
            regions: self.regions,
            atoms: self.atoms,
            frames: self.frames,
        }
    }

    /// Rounded background behind an inline code span, padded past the glyphs.
    fn code_pill(&self, pixmap: &mut Pixmap, span: Rect) {
        let pad = span.h * 0.2;
        let inset = span.h * 0.1;
        fill_round_rect(
            pixmap,
            span.x - pad,
            span.y + inset,
            span.w + pad * 2.0,
            span.h - inset * 2.0,
            span.h * 0.22,
            self.theme.inline_bg,
        );
    }

    fn paint_text(&mut self, pixmap: &mut Pixmap, text: TextOp) {
        let runs = collect_runs(&text.buffer);
        let offsets = line_offsets(&text.full);
        for run in &runs {
            let line_id = self.line_seq;
            self.line_seq += 1;
            let base = offsets.get(run.line_i).copied().unwrap_or(0);
            let mut atom: Option<Atom> = None;
            let mut link: Option<(String, Rect)> = None;
            let mut code: Option<Rect> = None;
            for glyph in &run.glyphs {
                let start = base + glyph.start;
                let info = text.spans.iter().find(|span| span.range.contains(&start));
                let rect = Rect {
                    x: text.x + glyph.x,
                    y: text.y + run.line_top,
                    w: glyph.w.max(0.4),
                    h: run.line_height.max(1.0),
                };
                if info.is_some_and(|span| span.code) {
                    code = Some(match code {
                        Some(pill) if (rect.x - (pill.x + pill.w)).abs() < 2.0 => pill.union(rect),
                        other => {
                            if let Some(pill) = other {
                                self.code_pill(pixmap, pill);
                            }
                            rect
                        }
                    });
                } else if let Some(pill) = code.take() {
                    self.code_pill(pixmap, pill);
                }
                if info.is_some_and(|span| span.strike) {
                    let mid = rect.y + rect.h * 0.55;
                    fill_rect(
                        pixmap,
                        rect.x,
                        mid,
                        rect.w,
                        1.0 * self.zoom,
                        self.theme.muted,
                    );
                }
                if let Some(url) = info.and_then(|span| span.link.clone()) {
                    match &mut link {
                        Some((current, region))
                            if *current == url && (rect.x - (region.x + region.w)).abs() < 3.0 =>
                        {
                            *region = region.union(rect);
                        }
                        other => {
                            if let Some((current, region)) = other.take() {
                                self.regions.push(Region {
                                    rect: region,
                                    kind: RegionKind::Link(current),
                                });
                            }
                            link = Some((url, rect));
                        }
                    }
                } else if let Some((current, region)) = link.take() {
                    self.regions.push(Region {
                        rect: region,
                        kind: RegionKind::Link(current),
                    });
                }
                let slice = run
                    .text
                    .get(glyph.start..glyph.end)
                    .unwrap_or("")
                    .to_owned();
                match &mut atom {
                    Some(current) if (rect.x - (current.rect.x + current.rect.w)).abs() < 2.0 => {
                        current.rect = current.rect.union(rect);
                        current.text.push_str(&slice);
                    }
                    other => {
                        if let Some(current) = other.take() {
                            self.atoms.push(current);
                        }
                        atom = Some(Atom {
                            rect,
                            text: slice,
                            line: line_id,
                        });
                    }
                }
            }
            if let Some(pill) = code {
                self.code_pill(pixmap, pill);
            }
            if let Some(current) = atom {
                self.atoms.push(current);
            }
            if let Some((current, region)) = link {
                self.regions.push(Region {
                    rect: region,
                    kind: RegionKind::Link(current),
                });
            }
        }

        let origin_x = text.x.round() as i32;
        let origin_y = text.y.round() as i32;
        text.buffer
            .draw(self.fonts, self.cache, text.color, |x, y, w, h, color| {
                let [r, g, b, a] = color.as_rgba();
                fill_rect(
                    pixmap,
                    (origin_x + x) as f32,
                    (origin_y + y) as f32,
                    w as f32,
                    h as f32,
                    [r, g, b, a],
                );
            });
    }
}

fn flatten(
    inlines: &[Inline],
    style: &SpanStyle,
    link_color: Rgba,
    code_color: Rgba,
) -> Vec<(String, SpanStyle)> {
    let mut out = Vec::new();
    flatten_into(inlines, style, link_color, code_color, &mut out);
    out
}

fn flatten_into(
    inlines: &[Inline],
    style: &SpanStyle,
    link_color: Rgba,
    code_color: Rgba,
    out: &mut Vec<(String, SpanStyle)>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => push_text(out, style, text),
            Inline::Code(text) => {
                let mut code = style.clone_style();
                code.mono = true;
                code.code = true;
                if style.link.is_none() {
                    code.color = code_color;
                }
                push_text(out, &code, text);
            }
            Inline::SoftBreak => push_text(out, style, " "),
            Inline::HardBreak => push_text(out, style, "\n"),
            Inline::Strong(children) => {
                let mut next = style.clone_style();
                next.weight = Weight::BOLD;
                flatten_into(children, &next, link_color, code_color, out);
            }
            Inline::Emphasis(children) => {
                let mut next = style.clone_style();
                next.font_style = FontStyle::Italic;
                flatten_into(children, &next, link_color, code_color, out);
            }
            Inline::Strike(children) => {
                let mut next = style.clone_style();
                next.strike = true;
                flatten_into(children, &next, link_color, code_color, out);
            }
            Inline::Link { dest, children } => {
                let mut next = style.clone_style();
                next.link = Some(dest.clone());
                next.color = link_color;
                flatten_into(children, &next, link_color, code_color, out);
            }
            Inline::Image { alt, dest, .. } => {
                let mut next = style.clone_style();
                next.link = Some(dest.clone());
                next.color = link_color;
                let label = if alt.is_empty() {
                    dest.clone()
                } else {
                    alt.clone()
                };
                push_text(out, &next, &label);
            }
        }
    }
}

impl SpanStyle {
    fn clone_style(&self) -> Self {
        Self {
            weight: self.weight,
            font_style: self.font_style,
            mono: self.mono,
            color: self.color,
            link: self.link.clone(),
            strike: self.strike,
            code: self.code,
            size: self.size,
        }
    }
}

fn push_text(out: &mut Vec<(String, SpanStyle)>, style: &SpanStyle, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some((last, last_style)) = out.last_mut() {
        if last_style.same(style) {
            last.push_str(text);
            return;
        }
    }
    out.push((text.to_owned(), style.clone_style()));
}

fn build_pieces(pieces: &[(String, SpanStyle)], metrics: Metrics) -> Vec<Piece> {
    let mut cursor = 0;
    let mut built = Vec::new();
    for (text, style) in pieces {
        if text.is_empty() {
            continue;
        }
        let start = cursor;
        cursor += text.len();
        let family = if style.mono { Family::Monospace } else { SANS };
        let attrs = Attrs::new()
            .family(family)
            .weight(style.weight)
            .style(style.font_style)
            .color(rgba_text(style.color))
            .metrics(Metrics::new(
                style.size.max(1.0),
                metrics.line_height.max(1.0),
            ));
        built.push(Piece {
            text: text.clone(),
            attrs,
            info: SpanInfo {
                range: start..cursor,
                link: style.link.clone(),
                strike: style.strike,
                code: style.code,
            },
        });
    }
    built
}

struct GlyphInfo {
    start: usize,
    end: usize,
    x: f32,
    w: f32,
}

struct RunInfo {
    line_i: usize,
    line_top: f32,
    line_height: f32,
    text: String,
    glyphs: Vec<GlyphInfo>,
}

fn collect_runs(buffer: &Buffer) -> Vec<RunInfo> {
    buffer
        .layout_runs()
        .map(|run| RunInfo {
            line_i: run.line_i,
            line_top: run.line_top,
            line_height: run.line_height,
            text: run.text.to_owned(),
            glyphs: run
                .glyphs
                .iter()
                .map(|glyph| GlyphInfo {
                    start: glyph.start,
                    end: glyph.end,
                    x: glyph.x,
                    w: glyph.w,
                })
                .collect(),
        })
        .collect()
}

fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

/// Anti-aliased rounded rectangle.
fn fill_round_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgba) {
    if w <= 0.0 || h <= 0.0 || color[3] == 0 {
        return;
    }
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = 0.552_284_8 * r;
    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    path.line_to(x, y + r);
    path.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    path.close();
    fill_path(pixmap, path, color);
}

fn fill_disclosure(pixmap: &mut Pixmap, x: f32, y: f32, size: f32, open: bool, color: Rgba) {
    let mut path = tiny_skia::PathBuilder::new();
    if open {
        path.move_to(x, y + size * 0.2);
        path.line_to(x + size, y + size * 0.2);
        path.line_to(x + size / 2.0, y + size * 0.9);
    } else {
        path.move_to(x + size * 0.15, y);
        path.line_to(x + size * 0.9, y + size / 2.0);
        path.line_to(x + size * 0.15, y + size);
    }
    path.close();
    fill_path(pixmap, path, color);
}

fn fill_path(pixmap: &mut Pixmap, path: tiny_skia::PathBuilder, color: Rgba) {
    let Some(path) = path.finish() else {
        return;
    };
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        tiny_skia::FillRule::Winding,
        tiny_skia::Transform::identity(),
        None,
    );
}

fn source_lines(literal: &str) -> Vec<&str> {
    if literal.is_empty() {
        return vec![""];
    }
    let mut split: Vec<&str> = literal.split('\n').collect();
    if literal.ends_with('\n') {
        split.pop();
    }
    if split.is_empty() {
        split.push("");
    }
    split
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TokenRole {
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
    Variable,
    Operator,
    Punctuation,
    Meta,
    Attribute,
}

fn token_color(scopes: &[Scope], colors: &SyntaxColors) -> Option<Rgba> {
    let role = token_role(scopes)?;
    Some(match role {
        TokenRole::Keyword => colors.keyword,
        TokenRole::String => colors.string,
        TokenRole::Number => colors.number,
        TokenRole::Comment => colors.comment,
        TokenRole::Function => colors.function,
        TokenRole::Type => colors.ty,
        TokenRole::Variable => colors.variable,
        TokenRole::Operator => colors.operator,
        TokenRole::Punctuation => colors.punctuation,
        TokenRole::Meta => colors.meta,
        TokenRole::Attribute => colors.attribute,
    })
}

fn token_role(scopes: &[Scope]) -> Option<TokenRole> {
    for scope in scopes.iter().rev() {
        let Some(role) = role_of(*scope) else {
            continue;
        };
        if role == TokenRole::Punctuation
            && let Some(parent) = scopes.iter().rev().find_map(|item| {
                let parent = role_of(*item)?;
                (parent != TokenRole::Punctuation).then_some(parent)
            })
        {
            return Some(parent);
        }
        return Some(role);
    }
    None
}

fn role_of(scope: Scope) -> Option<TokenRole> {
    fn rules() -> &'static [(Scope, TokenRole)] {
        static RULES: OnceLock<Vec<(Scope, TokenRole)>> = OnceLock::new();
        RULES.get_or_init(|| {
            [
                ("comment", TokenRole::Comment),
                ("string", TokenRole::String),
                ("constant.numeric", TokenRole::Number),
                ("constant.character", TokenRole::String),
                ("constant.other", TokenRole::Attribute),
                ("keyword.operator", TokenRole::Operator),
                ("storage.type", TokenRole::Type),
                ("storage", TokenRole::Keyword),
                ("keyword", TokenRole::Keyword),
                ("entity.name.function", TokenRole::Function),
                ("support.function", TokenRole::Function),
                ("variable.function", TokenRole::Function),
                ("entity.name.type", TokenRole::Type),
                ("entity.name.class", TokenRole::Type),
                ("support.type", TokenRole::Type),
                ("support.class", TokenRole::Type),
                ("variable.parameter", TokenRole::Attribute),
                ("variable", TokenRole::Variable),
                ("entity.name.tag", TokenRole::Meta),
                ("entity.other.attribute-name", TokenRole::Attribute),
                ("punctuation", TokenRole::Punctuation),
            ]
            .into_iter()
            .filter_map(|(name, role)| Some((Scope::new(name).ok()?, role)))
            .collect()
        })
    }
    rules()
        .iter()
        .find_map(|(prefix, role)| prefix.is_prefix_of(scope).then_some(*role))
}

fn resolved_palette(given: &[Option<(u8, u8, u8)>; 16], dark: bool) -> [(u8, u8, u8); 16] {
    let fallback = fallback_palette(dark);
    if given.iter().flatten().count() < 8 {
        return fallback;
    }
    let mut colors = fallback;
    for (slot, color) in colors.iter_mut().zip(given) {
        if let Some(color) = color {
            *slot = *color;
        }
    }
    colors
}

fn fallback_palette(dark: bool) -> [(u8, u8, u8); 16] {
    if dark {
        [
            (0x1d, 0x1f, 0x21),
            (0xcc, 0x66, 0x66),
            (0xb5, 0xbd, 0x68),
            (0xf0, 0xc6, 0x74),
            (0x81, 0xa2, 0xbe),
            (0xb2, 0x94, 0xbb),
            (0x8a, 0xbe, 0xb7),
            (0xc5, 0xc8, 0xc6),
            (0x66, 0x66, 0x66),
            (0xd5, 0x4e, 0x53),
            (0xb9, 0xca, 0x4a),
            (0xe7, 0xc5, 0x47),
            (0x7a, 0xa6, 0xda),
            (0xc3, 0x97, 0xd8),
            (0x70, 0xc0, 0xb1),
            (0xea, 0xea, 0xea),
        ]
    } else {
        [
            (0x24, 0x29, 0x2f),
            (0xcf, 0x22, 0x2e),
            (0x11, 0x63, 0x29),
            (0x4d, 0x2d, 0x00),
            (0x09, 0x69, 0xda),
            (0x82, 0x50, 0xdf),
            (0x1b, 0x7c, 0x83),
            (0x6e, 0x77, 0x81),
            (0x57, 0x60, 0x6a),
            (0xa4, 0x0e, 0x26),
            (0x1a, 0x7f, 0x37),
            (0x63, 0x3c, 0x01),
            (0x21, 0x8b, 0xff),
            (0xa4, 0x75, 0xf9),
            (0x31, 0x92, 0xaa),
            (0x8c, 0x95, 0x9f),
        ]
    }
}

/// Prefer the bright ANSI color when it is the same hue and easier to read.
fn hue_pick(normal: (u8, u8, u8), bright: (u8, u8, u8), bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (hue, sat, _) = hsl(normal);
    let (bright_hue, bright_sat, _) = hsl(bright);
    let same_hue = hue_distance(hue, bright_hue) <= 30.0;
    let saturated = bright_sat >= sat * 0.7;
    if same_hue && saturated && contrast(bright, bg) > contrast(normal, bg) {
        bright
    } else {
        normal
    }
}

fn ensure_contrast(color: Rgba, against: Rgba, minimum: f32, dark_bg: bool) -> Rgba {
    let toward = if dark_bg {
        [255, 255, 255, 255]
    } else {
        [0, 0, 0, 255]
    };
    let original = color;
    let mut amount = 0.0;
    let mut color = original;
    while contrast(rgb_of(color), rgb_of(against)) < minimum && amount < 0.85 {
        amount += 0.12;
        color = mix(original, toward, amount);
    }
    color
}

fn rgba((red, green, blue): (u8, u8, u8)) -> Rgba {
    [red, green, blue, 255]
}

fn rgb_of(color: Rgba) -> (u8, u8, u8) {
    (color[0], color[1], color[2])
}

fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (hi, lo) = (
        luminance(a).max(luminance(b)),
        luminance(a).min(luminance(b)),
    );
    (hi + 0.05) / (lo + 0.05)
}

fn luminance((red, green, blue): (u8, u8, u8)) -> f32 {
    let channel = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
}

fn hsl((red, green, blue): (u8, u8, u8)) -> (f32, f32, f32) {
    let (r, g, b) = (
        f32::from(red) / 255.0,
        f32::from(green) / 255.0,
        f32::from(blue) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta < 1e-5 {
        return (0.0, 0.0, lightness);
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let hue = if max == r {
        (g - b) / delta + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    (hue * 60.0, saturation, lightness)
}

fn hue_distance(a: f32, b: f32) -> f32 {
    let distance = (a - b).abs() % 360.0;
    distance.min(360.0 - distance)
}

fn table_header_bg(theme: &Theme) -> Rgba {
    mix(theme.bg, theme.fg, 0.08)
}

fn table_stripe(theme: &Theme) -> Rgba {
    mix(theme.bg, theme.fg, 0.045)
}

/// Fit columns to their text. A short column keeps its natural width; only the
/// widest columns shrink, and never below the longest word.
fn column_widths(min: &[f32], max: &[f32], available: f32) -> Vec<f32> {
    let mut widths = max.to_vec();
    loop {
        let sum: f32 = widths.iter().sum();
        if sum <= available + 0.5 {
            return widths;
        }
        let Some(index) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| **width > min[*index] + 0.5)
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index)
        else {
            return widths;
        };
        let room = widths[index] - min[index];
        widths[index] -= (sum - available).min(room);
    }
}

fn longest_word(inlines: &[Inline]) -> String {
    let mut best = String::new();
    fn walk(inlines: &[Inline], best: &mut String) {
        for inline in inlines {
            match inline {
                Inline::Text(text) | Inline::Code(text) | Inline::Image { alt: text, .. } => {
                    for word in text.split_whitespace() {
                        if word.chars().count() > best.chars().count() {
                            *best = word.to_owned();
                        }
                    }
                }
                Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strike(inner) => {
                    walk(inner, best);
                }
                Inline::Link { children, .. } => walk(children, best),
                Inline::SoftBreak | Inline::HardBreak => {}
            }
        }
    }
    walk(inlines, &mut best);
    best
}

fn mix(from: Rgba, to: Rgba, amount: f32) -> Rgba {
    let channel =
        |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    [
        channel(from[0], to[0]),
        channel(from[1], to[1]),
        channel(from[2], to[2]),
        255,
    ]
}

fn rgba_text(color: Rgba) -> TextColor {
    TextColor::rgba(color[0], color[1], color[2], color[3])
}

fn skia_color(color: Rgba) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color[0], color[1], color[2], color[3])
}

fn alert_color(kind: AlertKind) -> Rgba {
    match kind {
        AlertKind::Note => [88, 166, 255, 255],
        AlertKind::Tip => [63, 185, 80, 255],
        AlertKind::Important => [163, 113, 247, 255],
        AlertKind::Warning => [210, 153, 34, 255],
        AlertKind::Caution => [248, 81, 73, 255],
    }
}

fn paint_glyphs(pixmap: &mut Pixmap, fast: &FastText, inks: &[crate::fasttext::Ink]) {
    for ink in inks {
        let Some((width, height, coverage)) = fast.mask(ink.key) else {
            continue;
        };
        for row in 0..height {
            let row_start = (row * width) as usize;
            for col in 0..width {
                let alpha = coverage[row_start + col as usize];
                if alpha < 8 {
                    continue;
                }
                blend(
                    pixmap,
                    ink.x + col as i32,
                    ink.y + row as i32,
                    [ink.color[0], ink.color[1], ink.color[2], alpha],
                );
            }
        }
    }
}

pub(crate) fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
    if w <= 0.0 || h <= 0.0 || color[3] == 0 {
        return;
    }
    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = ((x + w).ceil() as u32).min(pixmap.width());
    let y1 = ((y + h).ceil() as u32).min(pixmap.height());
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    if color[3] == 255 {
        if let Some(pixel) = PremultipliedColorU8::from_rgba(color[0], color[1], color[2], 255) {
            let stride = pixmap.width() as usize;
            let pixels = pixmap.pixels_mut();
            for row in y0..y1 {
                let start = row as usize * stride + x0 as usize;
                let end = row as usize * stride + x1 as usize;
                pixels[start..end].fill(pixel);
            }
        }
        return;
    }
    for row in y0..y1 {
        for col in x0..x1 {
            blend(pixmap, col as i32, row as i32, color);
        }
    }
}

fn blend(pixmap: &mut Pixmap, x: i32, y: i32, color: Rgba) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    if x >= pixmap.width() || y >= pixmap.height() {
        return;
    }
    let index = (y * pixmap.width() + x) as usize;
    let pixel = &mut pixmap.pixels_mut()[index];
    let alpha = u16::from(color[3]);
    let inverse = 255 - alpha;
    let red = ((u16::from(color[0]) * alpha + u16::from(pixel.red()) * inverse) / 255) as u8;
    let green = ((u16::from(color[1]) * alpha + u16::from(pixel.green()) * inverse) / 255) as u8;
    let blue = ((u16::from(color[2]) * alpha + u16::from(pixel.blue()) * inverse) / 255) as u8;
    if let Some(next) = PremultipliedColorU8::from_rgba(red, green, blue, 255) {
        *pixel = next;
    }
}

fn blit(pixmap: &mut Pixmap, x: i32, y: i32, w: u32, h: u32, pixels: &[u8]) {
    for row in 0..h {
        for col in 0..w {
            let index = ((row * w + col) * 4) as usize;
            if index + 3 >= pixels.len() {
                return;
            }
            blend(
                pixmap,
                x + col as i32,
                y + row as i32,
                [
                    pixels[index],
                    pixels[index + 1],
                    pixels[index + 2],
                    pixels[index + 3],
                ],
            );
        }
    }
}

pub(crate) fn crop_pixmap(src: &Pixmap, x: u32, y: u32, w: u32, h: u32, bg: Rgba) -> Pixmap {
    let w = w.max(1);
    let h = h.max(1);
    let mut dst = Pixmap::new(w, h).unwrap_or_else(|| Pixmap::new(1, 1).expect("crop"));
    dst.fill(skia_color(bg));
    let copy_w = src.width().saturating_sub(x).min(w) as usize;
    let copy_h = src.height().saturating_sub(y).min(h) as usize;
    if copy_w == 0 || copy_h == 0 {
        return dst;
    }
    let src_stride = src.width() as usize;
    let dst_stride = w as usize;
    for row in 0..copy_h {
        let src_start = (y as usize + row) * src_stride + x as usize;
        let dst_start = row * dst_stride;
        dst.pixels_mut()[dst_start..dst_start + copy_w]
            .copy_from_slice(&src.pixels()[src_start..src_start + copy_w]);
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::parse;

    #[test]
    fn renders_a_clickable_link() {
        let doc = parse("See [docs](https://example.com) now.\n");
        let theme = Theme::dark();
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 800.0, &theme, 1.0, 16.0, Path::new("."));
        assert!(page.width() >= 800);
        assert!(page.height() > 20);
        assert!(page.regions.iter().any(|region| matches!(
            &region.kind,
            RegionKind::Link(url) if url == "https://example.com"
        )));
        let painted = page.pixmap.pixels().iter().any(|pixel| {
            pixel.red() != theme.bg[0]
                || pixel.green() != theme.bg[1]
                || pixel.blue() != theme.bg[2]
        });
        assert!(painted);
    }

    #[test]
    fn the_page_takes_the_terminal_background_when_it_matches_the_theme() {
        let terminal = (30, 32, 48);
        let theme = Theme::choose(ThemeChoice::Auto, Some(terminal), &[None; 16]);
        assert!(theme.dark);
        assert_eq!(theme.bg, [30, 32, 48, 255]);
        assert_ne!(theme.code_bg, theme.bg);
        assert_ne!(theme.syntax.string, theme.code_fg);
        let forced = Theme::choose(ThemeChoice::Light, Some(terminal), &[None; 16]);
        assert_eq!(forced.bg, Theme::light().bg);
    }

    #[test]
    fn a_full_width_image_uses_the_column_not_its_pixels() {
        let doc = parse(
            "<img src=\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==\" width=\"100%\" alt=\"shot\">\n",
        );
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 900.0, &Theme::dark(), 1.0, 16.0, Path::new("."));
        let region = page
            .regions
            .iter()
            .find(|region| matches!(region.kind, RegionKind::Link(_)))
            .expect("image");
        assert!(region.rect.w > 400.0, "column width, got {}", region.rect.w);
    }

    #[test]
    fn a_centred_title_keeps_the_rule_under_it() {
        let doc = parse("<div align=\"center\">\n\n<h1>md</h1>\n\n<p>Subtitle</p>\n\n</div>\n");
        let theme = Theme::dark();
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 900.0, &theme, 1.0, 16.0, Path::new("."));
        let title = page.atoms.iter().find(|atom| atom.text == "md").unwrap();
        let subtitle = page
            .atoms
            .iter()
            .find(|atom| atom.text == "Subtitle")
            .unwrap();
        let y0 = (title.rect.y + title.rect.h) as u32;
        let y1 = (subtitle.rect.y as u32).max(y0 + 1);
        let hits = (y0..y1)
            .map(|y| {
                (0..page.pixmap.width())
                    .filter(|x| {
                        page.pixmap.pixel(*x, y).is_some_and(|pixel| {
                            pixel.red() == theme.border[0] && pixel.green() == theme.border[1]
                        })
                    })
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert!(
            hits > 40,
            "rule under the title, hits {hits} between {y0} and {y1}"
        );
        let ink = ((title.rect.y as u32)..(title.rect.y + title.rect.h) as u32)
            .map(|y| {
                (title.rect.x as u32..(title.rect.x + title.rect.w) as u32)
                    .filter(|x| {
                        page.pixmap.pixel(*x, y).is_some_and(|pixel| {
                            pixel.red() != theme.bg[0] || pixel.green() != theme.bg[1]
                        })
                    })
                    .count()
            })
            .sum::<usize>();
        assert!(ink > 20, "glyphs in the title line, got {ink}");
    }

    #[test]
    fn shell_comments_and_flags_use_the_palette() {
        let theme = Theme::dark();
        let mut renderer = Renderer::new();
        let layouter = Layouter::new(&mut renderer, &theme, 1.0, 16.0, 800.0, Path::new("."));
        let lines = layouter.highlight("bash", "# note\nmd --help\n");
        let comment = lines[0]
            .iter()
            .find(|(text, _)| text.contains("note"))
            .expect("comment");
        assert_eq!(comment.1.color, theme.syntax.comment);
        let command = lines[1]
            .iter()
            .find(|(text, _)| text == "md")
            .expect("command");
        assert_eq!(command.1.color, theme.syntax.function);
        let flag = lines[1]
            .iter()
            .find(|(text, _)| text.contains("help"))
            .map(|(text, style)| {
                (
                    text.clone(),
                    style.color,
                    style.color == theme.syntax.attribute,
                )
            })
            .expect("flag");
        assert!(
            flag.2,
            "flag {:?} color {:?} attribute {:?}",
            flag.0, flag.1, theme.syntax.attribute
        );
    }

    #[test]
    fn centred_html_is_laid_out_centred() {
        let doc = parse("<p align=\"center\">Hi</p>\n\nLeft\n");
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 800.0, &Theme::dark(), 1.0, 16.0, Path::new("."));
        let op = page
            .atoms
            .iter()
            .find(|atom| atom.text == "Hi")
            .expect("centred text");
        let left = page
            .atoms
            .iter()
            .find(|atom| atom.text == "Left")
            .expect("left text");
        assert!(op.rect.x > left.rect.x + 100.0);
    }

    #[test]
    fn details_open_when_their_summary_is_clicked() {
        let doc = parse("<details>\n<summary>More</summary>\n\nHidden\n\n</details>\n");
        let mut renderer = Renderer::new();
        let render = |renderer: &mut Renderer| {
            renderer.render(&doc, 800.0, &Theme::dark(), 1.0, 16.0, Path::new("."))
        };
        let closed = render(&mut renderer);
        assert!(closed.atoms.iter().any(|atom| atom.text == "More"));
        assert!(!closed.atoms.iter().any(|atom| atom.text == "Hidden"));
        let summary = closed
            .atoms
            .iter()
            .find(|atom| atom.text == "More")
            .unwrap();
        let index =
            crate::page::hit_toggle(&closed.regions, summary.rect.x + 1.0, summary.rect.y + 1.0)
                .expect("summary toggles");

        renderer.toggle_details(index);
        let open = render(&mut renderer);
        assert!(open.atoms.iter().any(|atom| atom.text == "Hidden"));

        renderer.reset_details();
        assert!(
            !render(&mut renderer)
                .atoms
                .iter()
                .any(|atom| atom.text == "Hidden")
        );
    }

    #[test]
    fn rounded_fill_leaves_its_corners_clear() {
        let mut pixmap = Pixmap::new(40, 20).unwrap();
        fill_round_rect(&mut pixmap, 0.0, 0.0, 40.0, 20.0, 8.0, [255, 0, 0, 255]);
        assert_eq!(pixmap.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(pixmap.pixel(20, 10).unwrap().alpha(), 255);
    }

    #[test]
    fn a_changed_paragraph_sits_below_the_heading() {
        let before = "# Title\n\nKeep this.\n\nEdit me.\n";
        let after = "# Title\n\nKeep this.\n\nEdited.\n";
        let line = crate::change::first_changed_line(before, after).unwrap();
        let mut renderer = Renderer::new();
        let page = renderer.render(
            &parse(after),
            800.0,
            &Theme::dark(),
            1.0,
            16.0,
            Path::new("."),
        );
        let changed = crate::page::tightest_frame(&page.frames, line).unwrap();
        let heading = crate::page::tightest_frame(&page.frames, 1).unwrap();
        assert!(changed.rect.y > heading.rect.y);
        assert!(changed.rect.h > 1.0);
    }

    #[test]
    fn a_code_block_keeps_a_blank_line() {
        let doc = parse("```\nalpha\n\nbeta\n```\n");
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 800.0, &Theme::dark(), 1.0, 16.0, Path::new("."));
        let alpha = page.atoms.iter().find(|atom| atom.text == "alpha").unwrap();
        let beta = page.atoms.iter().find(|atom| atom.text == "beta").unwrap();
        assert!(beta.rect.y > alpha.rect.y + alpha.rect.h);
    }

    #[test]
    fn a_table_gives_the_short_column_less_room_and_rules_the_rows() {
        let doc = parse(
            "| Go | This explanation is long enough that it has to wrap inside the second column |\n| -- | -- |\n| ok | more words about the second column so the row is taller than the command |\n",
        );
        let theme = Theme::dark();
        let mut renderer = Renderer::new();
        let page = renderer.render(&doc, 900.0, &theme, 1.0, 16.0, Path::new("."));
        let go = page.atoms.iter().find(|atom| atom.text == "Go").unwrap();
        let explanation = page
            .atoms
            .iter()
            .find(|atom| atom.text.starts_with("This"))
            .unwrap();
        assert!(
            explanation.rect.x > go.rect.x + go.rect.w,
            "columns overlap"
        );
        assert!(
            explanation.rect.x < go.rect.x + 180.0,
            "short column took half the table: explanation at {}",
            explanation.rect.x - go.rect.x
        );
        let sample = page
            .pixmap
            .pixel((go.rect.x - 4.0) as u32, (go.rect.y + 2.0) as u32)
            .unwrap();
        let header = table_header_bg(&theme);
        assert_eq!(
            [sample.red(), sample.green(), sample.blue()],
            [header[0], header[1], header[2]]
        );
        let ok = page.atoms.iter().find(|atom| atom.text == "ok").unwrap();
        assert!(ok.rect.y > go.rect.y + go.rect.h);
        let hits = (go.rect.y as u32..ok.rect.y as u32)
            .map(|y| {
                (0..page.pixmap.width())
                    .filter(|x| {
                        page.pixmap.pixel(*x, y).is_some_and(|pixel| {
                            pixel.red() == theme.border[0] && pixel.green() == theme.border[1]
                        })
                    })
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert!(hits > 40, "row rule, hits {hits}");
    }
}
