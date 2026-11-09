//! Full-screen pager. Scroll, select, follow links, reload.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

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
use crate::protocol::{SourceRect, delete_image, place_image, transmit_png};
use crate::render::{Renderer, StatusBar, Theme, ThemeChoice, crop_pixmap, fill_rect};
use crate::terminal::{self as term, DisplayInfo, ScreenGuard};

/// Kitty image ids for the footer. The page uses 1 and 2.
pub const STATUS_IDS: [u32; 2] = [3, 4];
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
    shown: Option<Shown>,
    image_dirty: bool,
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
            shown: None,
            image_dirty: true,
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
            let wait = if self.renderer.images_pending() {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(150)
            };
            if !event::poll(wait)? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if self.on_key(key)? {
                        break;
                    }
                }
                Event::Mouse(mouse) => self.on_mouse(mouse)?,
                Event::Resize(cols, rows) => {
                    self.cols = cols.max(1);
                    self.rows = rows.max(1);
                    self.rerender();
                    self.present()?;
                }
                _ => {}
            }
        }
        Ok(())
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
        self.image_dirty = true;
        self.clamp();
    }

    fn present(&mut self) -> io::Result<()> {
        if self.help {
            return self.present_help();
        }
        let theme = Theme::choose(self.theme, self.background, &self.palette);
        let (image_cols, image_rows, view_w, view_h) = self.grid();
        let scroll_x = px(self.scroll_x);
        let scroll_y = px(self.scroll_y);
        let mut out = io::stdout().lock();
        // Placements are anchored at the cursor. Leave it at the top-left or
        // the page draws shifted and the right edge is clipped.
        execute!(out, MoveTo(0, 0))?;
        let can_pan = self.selection.is_none() && !self.image_dirty;
        let shown = self
            .shown
            .filter(|shown| can_pan && shown.covers(scroll_x, scroll_y, view_w, view_h));
        if let Some(shown) = shown {
            let (x, y, w, h) = shown.source(scroll_x, scroll_y, view_w, view_h);
            write!(
                out,
                "{}",
                place_image(
                    shown.id,
                    1,
                    SourceRect { x, y, w, h },
                    u32::from(image_cols),
                    u32::from(image_rows)
                )
            )?;
        } else {
            self.transmit(&mut out, &theme, image_cols, image_rows, view_w, view_h)?;
        }
        self.write_status(&mut out, &theme)?;
        Ok(())
    }

    /// Send the page pixels once. Later scrolls only move the source rectangle.
    fn transmit<W: Write>(
        &mut self,
        out: &mut W,
        theme: &Theme,
        cols: u16,
        rows: u16,
        view_w: u32,
        view_h: u32,
    ) -> io::Result<()> {
        let scroll_x = px(self.scroll_x);
        let scroll_y = px(self.scroll_y);
        let selecting = self.selection.is_some();
        let (mut image, origin_x, origin_y) = if selecting {
            let crop = crop_pixmap(
                &self.page.pixmap,
                scroll_x,
                scroll_y,
                view_w,
                view_h,
                theme.bg,
            );
            (crop, scroll_x, scroll_y)
        } else {
            let page_w = self.page.width().max(view_w);
            let page_h = self.page.height().max(view_h);
            let (origin_y, strip_h) = strip_window(scroll_y, view_h, page_h);
            let strip = crop_pixmap(&self.page.pixmap, 0, origin_y, page_w, strip_h, theme.bg);
            (strip, 0, origin_y)
        };
        self.paint_mark(&mut image, theme, origin_x as f32, origin_y as f32);
        if let Some(selection) = &self.selection {
            for atom in &self.page.atoms {
                if !atom.rect.intersects(selection.rect()) {
                    continue;
                }
                fill_rect(
                    &mut image,
                    atom.rect.x - origin_x as f32,
                    atom.rect.y - origin_y as f32,
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
        let source_x = scroll_x
            .saturating_sub(origin_x)
            .min(image.width().saturating_sub(1));
        let source_y = scroll_y
            .saturating_sub(origin_y)
            .min(image.height().saturating_sub(1));
        let source_w = view_w.min(image.width() - source_x).max(1);
        let source_h = view_h.min(image.height() - source_y).max(1);
        write!(out, "{}", transmit_png(next, &png))?;
        write!(
            out,
            "{}",
            place_image(
                next,
                1,
                SourceRect {
                    x: source_x,
                    y: source_y,
                    w: source_w,
                    h: source_h,
                },
                u32::from(cols),
                u32::from(rows)
            )
        )?;
        if self.slot != 0 {
            write!(out, "{}", delete_image(self.slot))?;
        }
        self.slot = next;
        if selecting {
            self.shown = None;
            self.image_dirty = true;
        } else {
            self.shown = Some(Shown {
                id: next,
                origin_y,
                width: image.width(),
                height: image.height(),
            });
            self.image_dirty = false;
        }
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
        self.shown = None;
        self.image_dirty = true;
        self.slot = 0;
        self.status_slot = 0;
        let mut out = io::stdout().lock();
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
        (self.page.height() as f32 - self.view_height()).max(0.0)
    }

    fn max_scroll_x(&self) -> f32 {
        (self.page.width() as f32 - self.grid().2 as f32).max(0.0)
    }

    fn clamp(&mut self) {
        self.scroll_x = self.scroll_x.clamp(0.0, self.max_scroll_x());
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y());
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

#[derive(Clone, Copy)]
struct Shown {
    id: u32,
    origin_y: u32,
    width: u32,
    height: u32,
}

impl Shown {
    fn covers(self, scroll_x: u32, scroll_y: u32, view_w: u32, view_h: u32) -> bool {
        scroll_x.saturating_add(view_w) <= self.width
            && scroll_y >= self.origin_y
            && scroll_y.saturating_add(view_h) <= self.origin_y.saturating_add(self.height)
    }

    fn source(
        self,
        scroll_x: u32,
        scroll_y: u32,
        view_w: u32,
        view_h: u32,
    ) -> (u32, u32, u32, u32) {
        let x = scroll_x.min(self.width.saturating_sub(1));
        let y = scroll_y
            .saturating_sub(self.origin_y)
            .min(self.height.saturating_sub(1));
        let w = view_w.min(self.width.saturating_sub(x)).max(1);
        let h = view_h.min(self.height.saturating_sub(y)).max(1);
        (x, y, w, h)
    }
}

fn px(value: f32) -> u32 {
    if value <= 0.0 { 0 } else { value as u32 }
}

/// Vertical slice of the page to upload. Scrolling inside it is only a place command.
fn strip_window(scroll: u32, view: u32, page: u32) -> (u32, u32) {
    let page = page.max(1);
    let view = view.clamp(1, page);
    let strip = view.saturating_mul(6).clamp(view, 8192).min(page);
    let mut origin = scroll.saturating_sub(strip.saturating_sub(view) / 2);
    let max_origin = page.saturating_sub(strip);
    if origin > max_origin {
        origin = max_origin;
    }
    (origin, strip)
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
    fn scrolling_inside_the_uploaded_page_does_not_need_a_new_image() {
        let (origin, strip) = strip_window(100, 500, 800);
        assert_eq!((origin, strip), (0, 800));
        let shown = Shown {
            id: 1,
            origin_y: origin,
            width: 1200,
            height: strip,
        };
        assert!(shown.covers(0, 100, 1200, 500));
        assert_eq!(shown.source(0, 100, 1200, 500), (0, 100, 1200, 500));

        let (origin, strip) = strip_window(5_000, 1_000, 20_000);
        let shown = Shown {
            id: 1,
            origin_y: origin,
            width: 800,
            height: strip,
        };
        assert!(shown.covers(0, 5_000, 800, 1_000));
        assert!(shown.covers(0, 6_000, 800, 1_000));
        assert!(!shown.covers(0, 0, 800, 1_000));
    }
}
