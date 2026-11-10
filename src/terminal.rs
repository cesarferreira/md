//! Terminal setup, kitty capability probe, and cleanup.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::protocol::{
    delete_image, has_attributes_reply, parse_background, parse_cell_size, parse_palette,
    parse_window_pixels, probe_query, supports_graphics,
};

pub struct DisplayInfo {
    pub cell_w: u32,
    pub cell_h: u32,
    pub background: Option<(u8, u8, u8)>,
    pub palette: [Option<(u8, u8, u8)>; 16],
}

pub fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// Point stdin at the controlling terminal after a piped document has been read.
pub fn attach_tty() -> io::Result<()> {
    if io::stdin().is_terminal() {
        return Ok(());
    }
    let tty = File::open("/dev/tty")?;
    // SAFETY: `tty` is a live fd and STDIN_FILENO is the process standard input.
    // dup2 replaces the pipe with the terminal so the pager can read keys.
    let rc = unsafe { libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn probe() -> io::Result<DisplayInfo> {
    let _raw = RawGuard::enter()?;
    let mut out = io::stdout();
    write!(out, "{}", probe_query())?;
    out.flush()?;
    let response = read_reply(Duration::from_millis(350));
    drop(_raw);
    if !supports_graphics(&response) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "kitty graphics protocol is not available",
        ));
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (cell_w, cell_h) = cell_size(&response, cols, rows);
    Ok(DisplayInfo {
        cell_w,
        cell_h,
        background: parse_background(&response),
        palette: parse_palette(&response),
    })
}

pub struct ScreenGuard;

impl ScreenGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        for id in [
            1,
            2,
            crate::pager::STATUS_IDS[0],
            crate::pager::STATUS_IDS[1],
        ] {
            let _ = write!(out, "{}", delete_image(id));
        }
        // Page tiles have ids this guard does not track. Free everything.
        let _ = write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\");
        let _ = write!(out, "\x1b[0m");
        let _ = execute!(out, Show, DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = out.flush();
    }
}

struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn cell_size(response: &str, cols: u16, rows: u16) -> (u32, u32) {
    if let Some((width, height)) = parse_cell_size(response) {
        if width > 0 && height > 0 {
            return (width, height);
        }
    }
    if let Some((width, height)) = parse_window_pixels(response) {
        let cols = u32::from(cols.max(1));
        let rows = u32::from(rows.max(1));
        return ((width / cols).max(1), (height / rows).max(1));
    }
    (9, 18)
}

fn read_reply(timeout: Duration) -> String {
    let mut bytes = Vec::new();
    let start = Instant::now();
    while start.elapsed() < timeout {
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = timeout.saturating_sub(start.elapsed()).as_millis().min(40) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, wait.max(1)) };
        if ready < 0 {
            break;
        }
        if ready == 0 {
            if response_ready(&bytes) {
                break;
            }
            continue;
        }
        let mut buf = [0u8; 2048];
        let count = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if count <= 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..count as usize]);
        if response_ready(&bytes) {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn response_ready(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    // The device-attributes reply is queried last, so everything the terminal
    // will answer has arrived once it is here.
    if has_attributes_reply(&text) {
        return true;
    }
    supports_graphics(&text)
        && (parse_cell_size(&text).is_some() || parse_window_pixels(&text).is_some())
        && parse_background(&text).is_some()
        && parse_palette(&text).iter().flatten().count() == 16
}
