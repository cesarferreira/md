//! Full-screen pager. Scroll, select, follow links, reload.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::change::first_changed_line;
use crate::document::parse;
use crate::nav::{Nav, resolve_link};
use crate::page::{
    Rect, anchor_y, change_scroll_y, frame_for_change, hit_link, hit_toggle, selected_text,
};
use crate::protocol::{
    SourceRect, delete_image, delete_placements, place_image, transmit_png,
};
use crate::render::{Renderer, StatusBar, Theme, ThemeChoice, crop_pixmap, fill_rect};
use crate::terminal::{self as term, DisplayInfo, ScreenGuard};

/// Kitty image ids for the footer. The selection view uses 1 and 2.
pub const STATUS_IDS: [u32; 2] = [3, 4];
/// First image id used for page tiles. Lower ids belong to the views above.
const TILE_ID_BASE: u32 = 100;
/// Target tile height in pixels, rounded down to whole cell rows. A tile is the
/// unit sent to the terminal, so a scroll uploads one tile, not the whole page.
const TILE_TARGET_PX: u32 = 768;
const STATUS_HINTS: &str = "j/k scroll   r reload   q quit   ? help";

const HELP: &[&str] = &[
    "md",
    "",
    "j/k or arrows     scroll",
    "d/u               half page",
    "space/b           page",
    "g/G               top / bottom",
    "h/l or arrows     scroll sideways",
    "click             open a link",
    "drag              select text",
    "y                 copy selection",
    "backspace         go back",
    "+/-/0             zoom",
    "saves             jump to what changed",
    "r                 reload now",
    "?                 toggle this help",
    "q                 quit",
];

struct Place {
    path: Option<PathBuf>,
    source: String,
    scroll_x: f32,
    scroll_y: f32,
}

struct Selection {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Selection {
    fn rect(&self) -> Rect {
        Rect {
            x: self.x0.min(self.x1),
            y: self.y0.min(self.y1),
            w: (self.x0 - self.x1).abs().max(1.0),
            h: (self.y0 - self.y1).abs().max(1.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

struct Watch {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<()>,
}

pub fn run(path: Option<PathBuf>, source: String, theme: ThemeChoice) -> Result<()> {
    if !term::stdout_is_terminal() {
        bail!("md draws with the kitty graphics protocol, so it has to run in a terminal");
    }
    term::attach_tty().context("could not open /dev/tty for keyboard input")?;
    let info = term::probe().map_err(|err| {
        if err.kind() == io::ErrorKind::Unsupported {
            anyhow::anyhow!(
                "md needs a terminal with the kitty graphics protocol (kitty, Ghostty, WezTerm)"
            )
        } else {
            anyhow::Error::from(err).context("failed to query the terminal")
        }
    })?;
    let _screen = ScreenGuard::enter()?;
    let mut app = App::new(path, source, theme, info)?;
    app.present()?;
    app.event_loop()
}

struct App {
    renderer: Renderer,
    path: Option<PathBuf>,
    source: String,
    page: crate::render::Page,
    scroll_x: f32,
    scroll_y: f32,
    zoom: f32,
    theme: ThemeChoice,
    background: Option<(u8, u8, u8)>,
    palette: [Option<(u8, u8, u8)>; 16],
    cell_w: u32,
    cell_h: u32,
    cols: u16,
    rows: u16,
    selection: Option<Selection>,
    anchor: Option<(f32, f32)>,
    hover: Option<String>,
    message: String,
    history: Vec<Place>,
    help: bool,
    slot: u32,
    status_slot: u32,
    stamp: Option<FileStamp>,
    watch: Option<Watch>,
    mark_line: Option<u32>,
    tiles: Tiles,
    image_dirty: bool,
    /// Column count the current page was laid out for.
    rendered_cols: u16,
    /// Set while the window is changing, so the reflow waits until it settles.
    resize_at: Option<Instant>,
    /// While set, `present` only records that a redraw is due.
    defer_present: bool,
    present_due: bool,
}

impl App {
    fn new(
        path: Option<PathBuf>,
        source: String,
        theme: ThemeChoice,
        info: DisplayInfo,
    ) -> Result<Self> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let mut app = Self {
            renderer: Renderer::new(),
            path,
            source,
            page: empty_page(),
            scroll_x: 0.0,
            scroll_y: 0.0,
            zoom: 1.0,
            theme,
            background: info.background,
            palette: info.palette,
            cell_w: info.cell_w.max(1),
            cell_h: info.cell_h.max(1),
            cols,
            rows,
            selection: None,
            anchor: None,
            hover: None,
            message: String::new(),
            history: Vec::new(),
            help: false,
            slot: 0,
            status_slot: 0,
            stamp: None,
            watch: None,
            mark_line: None,
            tiles: Tiles::default(),
            image_dirty: true,
            rendered_cols: cols,
            resize_at: None,
            defer_present: false,
            present_due: false,
        };
        app.note_stamp();
        app.rewatch();
        app.rerender();
        Ok(app)
    }

    fn event_loop(&mut self) -> Result<()> {
        loop {
            let mut dirty = self.refresh_if_changed();
            if self.renderer.take_image_updates() {
                self.rerender();
                dirty = true;
            }
            if dirty && !self.help {
                self.present()?;
            }
            // Poll quickly while badges are in flight so they appear with the page
            // instead of on the next keypress.
            let prefetching = self.next_prefetch().is_some();
            let wait = if self.resize_at.is_some() {
                Duration::from_millis(80)
            } else if self.renderer.images_pending() {
                Duration::from_millis(20)
            } else if prefetching {
                Duration::from_millis(2)
            } else {
                Duration::from_millis(150)
            };
            if !event::poll(wait)? {
                if self.resize_at.is_some() {
                    self.settle_resize()?;
                } else if prefetching {
                    self.prefetch()?;
                }
                continue;
            }
            // A trackpad flick queues dozens of events. Apply them all and draw
            // once, or the terminal falls behind on image uploads.
            self.defer_present = true;
            let quit = self.handle_queued();
            self.defer_present = false;
            let quit = quit?;
            if std::mem::take(&mut self.present_due) {
                self.present()?;
            }
            if quit {
                break;
            }
        }
        Ok(())
    }

    /// Handle the next event and every event already queued behind it.
    /// Returns true when the user asked to quit.
    fn handle_queued(&mut self) -> Result<bool> {
        loop {
            match event::read()? {
                Event::Resize(cols, rows) => self.note_resize(cols, rows)?,
                other => {
                    if self.resize_at.is_some() {
                        self.settle_resize()?;
                    }
                    match other {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            if self.on_key(key)? {
                                return Ok(true);
                            }
                        }
                        Event::Mouse(mouse) => self.on_mouse(mouse)?,
                        _ => {}
                    }
                }
            }
            if !event::poll(Duration::ZERO)? {
                return Ok(false);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.help
            && !matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
            )
        {
            self.help = false;
        }
        let half = self.view_height() / 2.0;
        let page = self.view_height();
        let line = self.cell_h as f32 * 2.0;
        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(true);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Esc => {
                if self.help {
                    self.help = false;
                } else if self.selection.take().is_some() {
                    self.message.clear();
                } else {
                    return Ok(true);
                }
            }
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_y += line,
            KeyCode::Char('k') | KeyCode::Up => self.scroll_y -= line,
            KeyCode::Char('d') => self.scroll_y += half,
            KeyCode::Char('u') => self.scroll_y -= half,
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_y += page,
            KeyCode::Char('b') | KeyCode::PageUp | KeyCode::Backspace
                if key.code != KeyCode::Backspace =>
            {
                self.scroll_y -= page;
            }
            KeyCode::Backspace => self.go_back(),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_y = 0.0,
            KeyCode::Char('G') | KeyCode::End => self.scroll_y = self.max_scroll_y(),
            KeyCode::Char('h') | KeyCode::Left => self.scroll_x -= 48.0,
            KeyCode::Char('l') | KeyCode::Right => self.scroll_x += 48.0,
            KeyCode::Char('y') => self.copy_selection(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.set_zoom(self.zoom + 0.1),
            KeyCode::Char('-') => self.set_zoom(self.zoom - 0.1),
            KeyCode::Char('0') => self.set_zoom(1.0),
            _ => return Ok(false),
        }
        self.clamp();
        self.present()?;
        Ok(false)
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.help {
            self.help = false;
            self.present()?;
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_y -= self.cell_h as f32 * 3.0,
            MouseEventKind::ScrollDown => self.scroll_y += self.cell_h as f32 * 3.0,
            MouseEventKind::ScrollLeft => self.scroll_x -= 48.0,
            MouseEventKind::ScrollRight => self.scroll_x += 48.0,
            MouseEventKind::Down(MouseButton::Left) => {
                let (x, y) = self.page_pos(mouse.column, mouse.row);
                self.anchor = Some((x, y));
                self.selection = None;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (x, y) = self.page_pos(mouse.column, mouse.row);
                if let Some((x0, y0)) = self.anchor {
                    self.selection = Some(Selection {
                        x0,
                        y0,
                        x1: x,
                        y1: y,
                    });
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let (x, y) = self.page_pos(mouse.column, mouse.row);
                let click = self.anchor.is_some_and(|(x0, y0)| {
                    (x - x0).abs() < self.cell_w as f32 && (y - y0).abs() < self.cell_h as f32
                });
                self.anchor = None;
                if click {
                    self.selection = None;
                    if mouse.row + 1 < self.rows {
                        if let Some(url) = hit_link(&self.page.regions, x, y) {
                            let url = url.to_owned();
                            self.activate(&url)?;
                        } else if let Some(index) = hit_toggle(&self.page.regions, x, y) {
                            self.renderer.toggle_details(index);
                            self.rerender();
                        }
                    }
                }
            }
            MouseEventKind::Moved => {
                let next = if mouse.row + 1 < self.rows {
                    let (x, y) = self.page_pos(mouse.column, mouse.row);
                    hit_link(&self.page.regions, x, y).map(str::to_owned)
                } else {
                    None
                };
                if next == self.hover {
                    return Ok(());
                }
                self.hover = next;
                self.draw_status()?;
                return Ok(());
            }
            _ => return Ok(()),
        }
        self.clamp();
        self.present()?;
        Ok(())
    }

    fn activate(&mut self, url: &str) -> Result<()> {
        match resolve_link(url, &self.base_dir()) {
            Some(Nav::External(url)) => match open::that(&url) {
                Ok(()) => self.message = format!("opened {url}"),
                Err(err) => self.message = format!("could not open {url}: {err}"),
            },
            Some(Nav::Scroll(anchor)) => {
                self.scroll_to(&anchor);
                self.message = format!("#{anchor}");
            }
            Some(Nav::File {
                path,
                anchor,
                markdown: true,
            }) => self.open_markdown(path, anchor)?,
            Some(Nav::File { path, .. }) => match open::that(&path) {
                Ok(()) => self.message = format!("opened {}", path.display()),
                Err(err) => {
                    self.message = format!("could not open {}: {err}", path.display());
                }
            },
            None => {}
        }
        Ok(())
    }

    fn open_markdown(&mut self, path: PathBuf, anchor: Option<String>) -> Result<()> {
        let source =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        self.history.push(self.place());
        self.path = Some(path);
        self.source = source;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.selection = None;
        self.mark_line = None;
        self.renderer.reset_details();
        self.note_stamp();
        self.rewatch();
        self.rerender();
        if let Some(anchor) = anchor.as_deref() {
            self.scroll_to(anchor);
        }
        self.message = self
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        Ok(())
    }

    fn go_back(&mut self) {
        let Some(place) = self.history.pop() else {
            self.message = "nothing to go back to".to_owned();
            return;
        };
        self.path = place.path;
        self.source = place.source;
        self.scroll_x = place.scroll_x;
        self.scroll_y = place.scroll_y;
        self.selection = None;
        self.mark_line = None;
        self.renderer.reset_details();
        self.rewatch();
        self.rerender();
        self.scroll_x = place.scroll_x;
        self.scroll_y = place.scroll_y;
        self.message = "back".to_owned();
    }

    fn place(&self) -> Place {
        Place {
            path: self.path.clone(),
            source: self.source.clone(),
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
        }
    }

    fn reload(&mut self) {
        let Some(path) = self.path.clone() else {
            self.message = "stdin cannot reload".to_owned();
            return;
        };
        match fs::read_to_string(&path) {
            Ok(text) => {
                self.stamp = file_stamp(&path);
                if text != self.source {
                    self.apply_source(text, "reloaded");
                }
            }
            Err(err) => self.message = format!("reload failed: {err}"),
        }
    }

    /// Re-read the open file when it was saved. Editors that replace the file
    /// via rename still show up, because the parent directory is watched and
    /// the mtime is checked on each tick.
    fn refresh_if_changed(&mut self) -> bool {
        let notified = self.drain_watch();
        let Some(path) = self.path.clone() else {
            return false;
        };
        let latest = file_stamp(&path);
        if !needs_reread(self.stamp, latest, notified) {
            return false;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            return false;
        };
        self.stamp = file_stamp(&path);
        if text == self.source {
            return false;
        }
        self.apply_source(text, "updated");
        true
    }

    fn apply_source(&mut self, text: String, message: &str) {
        let line = first_changed_line(&self.source, &text);
        let ratio = self.scroll_ratio();
        self.source = text;
        self.selection = None;
        self.rerender();
        match line.and_then(|line| change_scroll_y(&self.page.frames, line).map(|y| (line, y))) {
            Some((line, y)) => {
                self.scroll_y = y;
                self.mark_line = Some(line);
                self.message = format!("{message} · line {line}");
            }
            None => {
                self.mark_line = None;
                self.scroll_y = ratio * self.page.height() as f32;
                self.message = message.to_owned();
            }
        }
        self.clamp();
    }

    fn set_zoom(&mut self, zoom: f32) {
        let ratio = self.scroll_ratio();
        self.zoom = zoom.clamp(0.5, 3.0);
        self.selection = None;
        self.rerender();
        self.scroll_y = ratio * self.page.height() as f32;
    }

    fn scroll_to(&mut self, anchor: &str) {
        if let Some(y) = anchor_y(&self.page.regions, anchor) {
            self.scroll_y = (y - 12.0).max(0.0);
        } else {
            self.message = format!("missing #{anchor}");
        }
    }

    fn copy_selection(&mut self) {
        let Some(selection) = &self.selection else {
            self.message = "no selection".to_owned();
            return;
        };
        let text = selected_text(&self.page.atoms, selection.rect());
        if text.is_empty() {
            self.message = "no selection".to_owned();
            return;
        }
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.clone())) {
            Ok(()) => self.message = format!("copied {} characters", text.chars().count()),
            Err(err) => self.message = format!("copy failed: {err}"),
        }
    }

    fn rerender(&mut self) {
        let doc = parse(&self.source);
        let width = f32::from(self.cols.max(1)) * self.cell_w as f32;
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let base = self.base_dir();
        let text_px = (self.cell_h as f32 * 0.8).clamp(16.0, 40.0);
        self.page = self
            .renderer
            .render(&doc, width, &theme, self.zoom, text_px, &base);
        self.rendered_cols = self.cols;
        self.image_dirty = true;
        self.clamp();
    }

    /// Track the new size and stretch the current picture to fill it. The
    /// reflow happens once the resize stops, not on every event in the drag.
    fn note_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.resize_at = Some(Instant::now());
        self.restretch()
    }

    fn settle_resize(&mut self) -> io::Result<()> {
        if self.resize_at.take().is_none() {
            return Ok(());
        }
        if self.cols != self.rendered_cols {
            self.rerender();
        }
        self.present()
    }

    /// Move the tiles already on screen to the new window, without encoding pixels.
    fn restretch(&mut self) -> io::Result<()> {
        if self.help {
            return Ok(());
        }
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let mut out = io::stdout().lock();
        if self.selection.is_none() {
            self.place_tiles(&mut out)?;
        }
        self.write_status(&mut out, &theme)?;
        Ok(())
    }

    fn present(&mut self) -> io::Result<()> {
        if self.defer_present {
            self.present_due = true;
            return Ok(());
        }
        self.present_now()
    }

    fn present_now(&mut self) -> io::Result<()> {
        if self.help {
            return self.present_help();
        }
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let mut out = io::stdout().lock();
        if self.selection.is_some() {
            self.hide_tiles(&mut out)?;
            self.present_selection(&mut out, &theme)?;
        } else {
            if self.slot != 0 {
                write!(out, "{}", delete_image(self.slot))?;
                self.slot = 0;
            }
            if self.image_dirty {
                self.purge_tiles(&mut out)?;
                self.image_dirty = false;
            }
            let (_, _, _, view_h) = self.grid();
            let (tile_px, _, count) = self.page_metrics();
            let scroll_y = px(self.scroll_y);
            for index in visible_tiles(scroll_y, view_h, tile_px, count) {
                if !self.tiles.cache.contains_key(&index) {
                    self.upload_tile(&mut out, &theme, index)?;
                }
            }
            self.place_tiles(&mut out)?;
            self.evict_tiles(&mut out)?;
        }
        self.write_status(&mut out, &theme)?;
        Ok(())
    }

    /// Page height in pixels rounded up to whole cell rows. The page ends on a
    /// cell edge so the last tile and the last scroll position line up.
    fn page_metrics(&self) -> (u32, u32, u32) {
        let cell = self.cell_h.max(1);
        let (_, _, _, view_h) = self.grid();
        let tile_px = tile_rows(cell) * cell;
        let padded = padded_height(self.page.height(), view_h, cell);
        (tile_px, padded, padded.div_ceil(tile_px))
    }

    /// Encode one tile of the page and send it to the terminal, without placing it.
    fn upload_tile<W: Write>(&mut self, out: &mut W, theme: &Theme, index: u32) -> io::Result<()> {
        let (_, _, view_w, _) = self.grid();
        let (tile_px, padded, _) = self.page_metrics();
        let (y, h) = tile_span(index, tile_px, padded);
        let width = self.page.width().max(view_w);
        let mut image = crop_pixmap(&self.page.pixmap, 0, y, width, h, theme.bg);
        self.paint_mark(&mut image, theme, 0.0, y as f32);
        let png = image
            .encode_png()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let id = TILE_ID_BASE + self.tiles.next_id;
        self.tiles.next_id += 1;
        write!(out, "{}", transmit_png(id, &png))?;
        self.tiles.tick += 1;
        self.tiles.cache.insert(
            index,
            Tile {
                id,
                width,
                used: self.tiles.tick,
            },
        );
        Ok(())
    }

    /// Place every visible tile that has been uploaded and lift the placements
    /// of tiles that scrolled away. The image data stays in the terminal.
    fn place_tiles<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let (cols, rows, view_w, view_h) = self.grid();
        let (tile_px, padded, count) = self.page_metrics();
        let cell_h = self.cell_h.max(1);
        let scroll_x = px(self.scroll_x);
        let scroll_y = px(self.scroll_y);
        let mut now = HashSet::new();
        for index in visible_tiles(scroll_y, view_h, tile_px, count) {
            let Some(tile) = self.tiles.cache.get_mut(&index) else {
                continue;
            };
            let (_, tile_h) = tile_span(index, tile_px, padded);
            let Some(spot) = place_tile(index, scroll_y, view_h, tile_px, tile_h) else {
                continue;
            };
            self.tiles.tick += 1;
            tile.used = self.tiles.tick;
            let x = scroll_x.min(tile.width.saturating_sub(1));
            let w = view_w.min(tile.width - x).max(1);
            execute!(out, MoveTo(0, (spot.top / cell_h) as u16))?;
            write!(
                out,
                "{}",
                place_image(
                    tile.id,
                    1,
                    SourceRect {
                        x,
                        y: spot.src_y,
                        w,
                        h: spot.height,
                    },
                    cells_for(w, self.cell_w, cols),
                    cells_for(spot.height, cell_h, rows)
                )
            )?;
            now.insert(index);
        }
        for index in self.tiles.placed.difference(&now) {
            if let Some(tile) = self.tiles.cache.get(index) {
                write!(out, "{}", delete_placements(tile.id))?;
            }
        }
        self.tiles.placed = now;
        Ok(())
    }

    /// Lift every tile placement, keeping the uploaded data for later.
    fn hide_tiles<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        for index in std::mem::take(&mut self.tiles.placed) {
            if let Some(tile) = self.tiles.cache.get(&index) {
                write!(out, "{}", delete_placements(tile.id))?;
            }
        }
        Ok(())
    }

    /// Free every tile in the terminal. Used when the page pixels change.
    fn purge_tiles<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        for tile in self.tiles.cache.values() {
            write!(out, "{}", delete_image(tile.id))?;
        }
        self.tiles.cache.clear();
        self.tiles.placed.clear();
        Ok(())
    }

    /// Tiles worth holding: the view plus some room ahead and behind.
    fn keep_range(&self) -> Range<u32> {
        let (_, _, _, view_h) = self.grid();
        let (tile_px, _, count) = self.page_metrics();
        keep_tiles(px(self.scroll_y), view_h, tile_px, count)
    }

    /// Free the tiles farthest from the view once the cache is over its budget.
    fn evict_tiles<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let keep = self.keep_range();
        let limit = keep.len() + 2;
        if self.tiles.cache.len() <= limit {
            return Ok(());
        }
        let (tile_px, _, _) = self.page_metrics();
        let center = px(self.scroll_y) / tile_px.max(1);
        let mut far: Vec<u32> = self
            .tiles
            .cache
            .keys()
            .copied()
            .filter(|index| !keep.contains(index))
            .collect();
        far.sort_by_key(|index| std::cmp::Reverse(index.abs_diff(center)));
        let excess = self.tiles.cache.len() - limit;
        for index in far.into_iter().take(excess) {
            if let Some(tile) = self.tiles.cache.remove(&index) {
                write!(out, "{}", delete_image(tile.id))?;
            }
            self.tiles.placed.remove(&index);
        }
        Ok(())
    }

    /// The nearest tile in the keep range that the terminal does not have yet.
    fn next_prefetch(&self) -> Option<u32> {
        if self.help || self.selection.is_some() || self.image_dirty {
            return None;
        }
        let (tile_px, _, _) = self.page_metrics();
        let center = px(self.scroll_y) / tile_px.max(1);
        self.keep_range()
            .filter(|index| !self.tiles.cache.contains_key(index))
            .min_by_key(|index| index.abs_diff(center))
    }

    /// Upload one tile ahead of the scroll while the user is idle.
    fn prefetch(&mut self) -> io::Result<()> {
        let Some(index) = self.next_prefetch() else {
            return Ok(());
        };
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let mut out = io::stdout().lock();
        self.upload_tile(&mut out, &theme, index)?;
        out.flush()
    }

    /// While text is selected the highlight is painted into one picture of the
    /// view, so the tiles are lifted and this picture replaces them.
    fn present_selection<W: Write>(&mut self, out: &mut W, theme: &Theme) -> io::Result<()> {
        let (cols, rows, view_w, view_h) = self.grid();
        let scroll_x = px(self.scroll_x);
        let scroll_y = px(self.scroll_y);
        execute!(out, MoveTo(0, 0))?;
        let mut image = crop_pixmap(
            &self.page.pixmap,
            scroll_x,
            scroll_y,
            view_w,
            view_h,
            theme.bg,
        );
        self.paint_mark(&mut image, theme, scroll_x as f32, scroll_y as f32);
        if let Some(selection) = &self.selection {
            for atom in &self.page.atoms {
                if !atom.rect.intersects(selection.rect()) {
                    continue;
                }
                fill_rect(
                    &mut image,
                    atom.rect.x - scroll_x as f32,
                    atom.rect.y - scroll_y as f32,
                    atom.rect.w,
                    atom.rect.h,
                    theme.selection,
                );
            }
        }
        let png = image
            .encode_png()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let next = if self.slot == 1 { 2 } else { 1 };
        write!(out, "{}", transmit_png(next, &png))?;
        write!(
            out,
            "{}",
            place_image(
                next,
                1,
                SourceRect {
                    x: 0,
                    y: 0,
                    w: image.width(),
                    h: image.height(),
                },
                u32::from(cols),
                u32::from(rows)
            )
        )?;
        if self.slot != 0 {
            write!(out, "{}", delete_image(self.slot))?;
        }
        self.slot = next;
        Ok(())
    }

    fn paint_mark(
        &self,
        crop: &mut tiny_skia::Pixmap,
        theme: &Theme,
        origin_x: f32,
        origin_y: f32,
    ) {
        let Some(line) = self.mark_line else {
            return;
        };
        let Some(frame) = frame_for_change(&self.page.frames, line) else {
            return;
        };
        let wash = [theme.accent[0], theme.accent[1], theme.accent[2], 56];
        fill_rect(
            crop,
            frame.rect.x - origin_x,
            frame.rect.y - origin_y,
            frame.rect.w,
            frame.rect.h,
            wash,
        );
        fill_rect(
            crop,
            frame.rect.x - origin_x,
            frame.rect.y - origin_y,
            4.0,
            frame.rect.h,
            theme.accent,
        );
    }

    fn present_help(&mut self) -> io::Result<()> {
        self.image_dirty = true;
        self.slot = 0;
        self.status_slot = 0;
        let mut out = io::stdout().lock();
        self.purge_tiles(&mut out)?;
        for id in [1, 2, STATUS_IDS[0], STATUS_IDS[1]] {
            write!(out, "{}", delete_image(id))?;
        }
        execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;
        for (index, line) in HELP.iter().enumerate() {
            execute!(out, MoveTo(2, index as u16 + 1), Print(line))?;
        }
        Ok(())
    }

    /// Directory (with `~` for home) and file name for the footer.
    fn title(&self) -> (String, String) {
        let Some(path) = &self.path else {
            return (String::new(), "stdin".to_owned());
        };
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let dir = std::path::absolute(path)
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .map(|dir| home_relative(&dir))
            .unwrap_or_default();
        (dir, file)
    }

    fn status_note(&self) -> String {
        let mut note = self.hover.clone().unwrap_or_else(|| self.message.clone());
        let zoom = (self.zoom * 100.0).round() as u32;
        if zoom != 100 {
            if !note.is_empty() {
                note.push_str("   ");
            }
            note.push_str(&format!("{zoom}%"));
        }
        note
    }

    fn progress(&self) -> f32 {
        let page = self.page.height() as f32;
        if page <= self.view_height() {
            1.0
        } else {
            ((self.scroll_y + self.view_height()) / page).clamp(0.0, 1.0)
        }
    }

    fn draw_status(&mut self) -> io::Result<()> {
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let mut out = io::stdout().lock();
        self.write_status(&mut out, &theme)
    }

    fn write_status<W: Write>(&mut self, out: &mut W, theme: &Theme) -> io::Result<()> {
        let (cols, _, view_w, _) = self.grid();
        let (dir, file) = self.title();
        let note = self.status_note();
        let bar = StatusBar {
            dir: &dir,
            file: &file,
            note: &note,
            hints: STATUS_HINTS,
            progress: self.progress(),
        };
        let pixmap = self.renderer.status_bar(&bar, view_w, self.cell_h, theme);
        let png = pixmap
            .encode_png()
            .map_err(|err| io::Error::other(err.to_string()))?;
        let next = if self.status_slot == STATUS_IDS[0] {
            STATUS_IDS[1]
        } else {
            STATUS_IDS[0]
        };
        execute!(out, MoveTo(0, self.rows.saturating_sub(1)))?;
        write!(out, "{}", transmit_png(next, &png))?;
        write!(
            out,
            "{}",
            place_image(
                next,
                1,
                SourceRect {
                    x: 0,
                    y: 0,
                    w: pixmap.width(),
                    h: pixmap.height(),
                },
                u32::from(cols),
                1
            )
        )?;
        if self.status_slot != 0 {
            write!(out, "{}", delete_image(self.status_slot))?;
        }
        self.status_slot = next;
        out.flush()
    }

    fn grid(&self) -> (u16, u16, u32, u32) {
        let cols = self.cols.max(1);
        let rows = self.rows.saturating_sub(1).max(1);
        let width = u32::from(cols) * self.cell_w;
        let height = u32::from(rows) * self.cell_h;
        (cols, rows, width.max(1), height.max(1))
    }

    fn view_height(&self) -> f32 {
        self.grid().3 as f32
    }

    fn max_scroll_y(&self) -> f32 {
        let (_, padded, _) = self.page_metrics();
        padded.saturating_sub(self.grid().3) as f32
    }

    fn max_scroll_x(&self) -> f32 {
        (self.page.width() as f32 - self.grid().2 as f32).max(0.0)
    }

    fn clamp(&mut self) {
        self.scroll_x = self.scroll_x.clamp(0.0, self.max_scroll_x());
        self.scroll_y = snap_scroll(self.scroll_y, self.cell_h as f32, self.max_scroll_y());
    }

    fn scroll_ratio(&self) -> f32 {
        let height = self.page.height() as f32;
        if height <= 1.0 {
            0.0
        } else {
            self.scroll_y / height
        }
    }

    fn page_pos(&self, column: u16, row: u16) -> (f32, f32) {
        (
            f32::from(column) * self.cell_w as f32 + self.scroll_x,
            f32::from(row) * self.cell_h as f32 + self.scroll_y,
        )
    }

    fn base_dir(&self) -> PathBuf {
        self.path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    fn note_stamp(&mut self) {
        self.stamp = self.path.as_deref().and_then(file_stamp);
    }

    fn rewatch(&mut self) {
        self.watch = self.path.as_deref().and_then(watch_file);
    }

    fn drain_watch(&self) -> bool {
        let Some(watch) = &self.watch else {
            return false;
        };
        let mut changed = false;
        while watch.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

fn needs_reread(seen: Option<FileStamp>, latest: Option<FileStamp>, notified: bool) -> bool {
    notified || latest != seen
}

fn affects_file(paths: &[PathBuf], file_name: &std::ffi::OsStr) -> bool {
    paths.is_empty() || paths.iter().any(|path| path.file_name() == Some(file_name))
}

fn watch_file(path: &Path) -> Option<Watch> {
    let file_name = path.file_name()?.to_os_string();
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        if affects_file(&event.paths, &file_name) {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some(Watch {
        _watcher: watcher,
        rx,
    })
}

fn empty_page() -> crate::render::Page {
    crate::render::Page {
        pixmap: tiny_skia::Pixmap::new(1, 1).expect("pixmap"),
        regions: Vec::new(),
        atoms: Vec::new(),
        frames: Vec::new(),
    }
}

fn home_relative(dir: &Path) -> String {
    let shown = dir.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

struct Tile {
    id: u32,
    width: u32,
    used: u64,
}

/// Page tiles the terminal holds, by tile index.
#[derive(Default)]
struct Tiles {
    cache: HashMap<u32, Tile>,
    /// Tiles with a live placement on screen.
    placed: HashSet<u32>,
    next_id: u32,
    tick: u64,
}

/// Cell rows in one tile, so every tile edge lands on a cell boundary.
fn tile_rows(cell_h: u32) -> u32 {
    (TILE_TARGET_PX / cell_h.max(1)).max(1)
}

/// Page height rounded up to whole cells, and never shorter than the view.
fn padded_height(page_h: u32, view_h: u32, cell_h: u32) -> u32 {
    (page_h.div_ceil(cell_h.max(1)) * cell_h.max(1)).max(view_h)
}

/// First pixel row and height of a tile. The last tile may be shorter.
fn tile_span(index: u32, tile_px: u32, padded: u32) -> (u32, u32) {
    let top = index * tile_px;
    (top, tile_px.min(padded.saturating_sub(top)))
}

/// Tiles that overlap the view.
fn visible_tiles(scroll: u32, view_h: u32, tile_px: u32, count: u32) -> Range<u32> {
    let tile_px = tile_px.max(1);
    let first = (scroll / tile_px).min(count);
    let last = ((scroll + view_h).saturating_sub(1) / tile_px + 1).min(count);
    first..last.max(first)
}

/// Tiles worth keeping: the view, three quarters of a view behind, and a view
/// and three quarters ahead. Reading goes down, so more room lies that way.
fn keep_tiles(scroll: u32, view_h: u32, tile_px: u32, count: u32) -> Range<u32> {
    let tile_px = tile_px.max(1);
    let lo = scroll.saturating_sub(view_h * 3 / 4);
    let hi = scroll + view_h * 7 / 4;
    let first = (lo / tile_px).min(count);
    let last = (hi.saturating_sub(1) / tile_px + 1).min(count);
    first..last.max(first)
}

/// Where the visible part of a tile goes: pixel rows below the top of the view,
/// the first source row inside the tile, and the visible height.
#[derive(Debug, PartialEq, Eq)]
struct Spot {
    top: u32,
    src_y: u32,
    height: u32,
}

fn place_tile(index: u32, scroll: u32, view_h: u32, tile_px: u32, tile_h: u32) -> Option<Spot> {
    let tile_top = index * tile_px;
    let top = tile_top.max(scroll);
    let bottom = (tile_top + tile_h).min(scroll + view_h);
    (bottom > top).then_some(Spot {
        top: top - scroll,
        src_y: top - tile_top,
        height: bottom - top,
    })
}

/// Vertical scroll is kept on whole cell rows so tile edges stay on cell edges.
/// Rounds down so a jump target is never cut off at the top.
fn snap_scroll(scroll: f32, cell: f32, max: f32) -> f32 {
    let cell = cell.max(1.0);
    let snapped = ((scroll / cell) + 1e-3).floor() * cell;
    snapped.clamp(0.0, max.max(0.0))
}

fn px(value: f32) -> u32 {
    if value <= 0.0 { 0 } else { value as u32 }
}

/// Cell count that displays `pixels` at the terminal's cell size, never past the window.
fn cells_for(pixels: u32, cell: u32, window: u16) -> u32 {
    let count = (pixels as f32 / cell.max(1) as f32).round() as u32;
    count.clamp(1, u32::from(window.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64) -> FileStamp {
        FileStamp {
            modified: None,
            len,
        }
    }

    #[test]
    fn a_save_or_a_notify_rereads_the_file() {
        let seen = Some(stamp(1));
        assert!(!needs_reread(seen, Some(stamp(1)), false));
        assert!(needs_reread(seen, Some(stamp(2)), false));
        assert!(needs_reread(seen, Some(stamp(1)), true));
        assert!(needs_reread(seen, None, false));
    }

    #[test]
    fn directory_events_for_other_files_are_ignored() {
        let name = std::ffi::OsStr::new("readme.md");
        assert!(affects_file(&[], name));
        assert!(affects_file(&[PathBuf::from("/tmp/readme.md")], name));
        assert!(!affects_file(&[PathBuf::from("/tmp/other.md")], name));
    }

    #[test]
    fn tiles_are_whole_cell_rows() {
        // 768 px target at 20 px cells: 38 rows, 760 px.
        assert_eq!(tile_rows(20), 38);
        assert_eq!(tile_rows(18) * 18, 756);
        // A cell taller than the target still gives one row.
        assert_eq!(tile_rows(900), 1);
    }

    #[test]
    fn the_page_is_padded_to_a_cell_edge_and_at_least_a_view() {
        assert_eq!(padded_height(1001, 400, 20), 1020);
        assert_eq!(padded_height(1000, 400, 20), 1000);
        assert_eq!(padded_height(100, 400, 20), 400);
    }

    #[test]
    fn only_tiles_that_overlap_the_view_are_visible() {
        // Tiles of 100 px, five of them.
        assert_eq!(visible_tiles(0, 250, 100, 5), 0..3);
        assert_eq!(visible_tiles(100, 100, 100, 5), 1..2);
        assert_eq!(visible_tiles(150, 100, 100, 5), 1..3);
        assert_eq!(visible_tiles(400, 100, 100, 5), 4..5);
        // A view past the end never asks for tiles that do not exist.
        assert_eq!(visible_tiles(450, 200, 100, 5), 4..5);
    }

    #[test]
    fn the_keep_range_covers_the_view_with_more_room_below() {
        let keep = keep_tiles(1000, 400, 100, 40);
        assert!(keep.start <= 10 && keep.end >= 14);
        assert!(1000 - keep.start * 100 < 400);
        assert!(keep.end * 100 - 1400 >= 300);
        assert_eq!(keep_tiles(0, 400, 100, 3), 0..3);
    }

    #[test]
    fn a_partly_visible_tile_is_cut_to_the_view() {
        // View 400 px at scroll 150, tiles of 100 px.
        let spot = |index| place_tile(index, 150, 400, 100, 100);
        assert_eq!(spot(0), None);
        assert_eq!(
            spot(1),
            Some(Spot {
                top: 0,
                src_y: 50,
                height: 50
            })
        );
        assert_eq!(
            spot(2),
            Some(Spot {
                top: 50,
                src_y: 0,
                height: 100
            })
        );
        assert_eq!(
            spot(5),
            Some(Spot {
                top: 350,
                src_y: 0,
                height: 50
            })
        );
        assert_eq!(spot(6), None);
    }

    #[test]
    fn visible_tiles_fill_the_view_with_no_gap_or_overlap() {
        let (view, tile, page) = (400, 100, 1000);
        for scroll in (0..=600).step_by(20) {
            let mut next = 0;
            for index in visible_tiles(scroll, view, tile, page / tile) {
                let spot = place_tile(index, scroll, view, tile, tile).expect("visible");
                assert_eq!(spot.top, next, "scroll {scroll} tile {index}");
                next += spot.height;
            }
            assert_eq!(next, view, "scroll {scroll}");
        }
    }

    #[test]
    fn scroll_lands_on_cell_rows_and_never_past_the_ends() {
        assert_eq!(snap_scroll(61.0, 20.0, 1000.0), 60.0);
        assert_eq!(snap_scroll(60.0, 20.0, 1000.0), 60.0);
        assert_eq!(snap_scroll(-5.0, 20.0, 1000.0), 0.0);
        assert_eq!(snap_scroll(5000.0, 20.0, 1000.0), 1000.0);
        // Float error from accumulated scrolling must not drop a whole row.
        assert_eq!(snap_scroll(59.9999, 20.0, 1000.0), 60.0);
    }
}
