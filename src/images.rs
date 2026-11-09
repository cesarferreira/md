//! Image loading. Local files and data URLs load on demand; remote URLs are
//! fetched on a background thread so the page can draw before they arrive.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use image::RgbaImage;
use resvg::{tiny_skia, usvg};

/// SVGs are rasterised larger than their display size so the later resize
/// stays sharp at any zoom.
const SVG_OVERSAMPLE: f32 = 4.0;
const MAX_SVG_PIXELS: f32 = 4096.0;

/// Decoded pixels plus the image's own size in CSS pixels.
pub struct Picture {
    pub pixels: RgbaImage,
    pub width: f32,
    pub height: f32,
}

pub enum Lookup {
    Ready(Arc<Picture>),
    Pending,
    Missing,
}

enum Entry {
    Pending,
    Ready(Arc<Picture>),
    Failed,
}

#[derive(Clone, Default)]
pub struct ImageStore {
    remote: Arc<Mutex<HashMap<String, Entry>>>,
    updated: Arc<AtomicBool>,
}

impl ImageStore {
    pub fn get(&self, dest: &str, base_dir: &Path) -> Lookup {
        if dest.starts_with("http://") || dest.starts_with("https://") {
            return self.remote(dest);
        }
        let picture = if let Some(data) = dest.strip_prefix("data:") {
            decode_data_url(data)
        } else if dest.is_empty() {
            None
        } else {
            let path = Path::new(dest);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(path)
            };
            fs::read(path).ok().and_then(|bytes| decode(&bytes))
        };
        picture.map_or(Lookup::Missing, |picture| Lookup::Ready(Arc::new(picture)))
    }

    /// True once since the last call if a remote image finished loading.
    pub fn take_updated(&self) -> bool {
        self.updated.swap(false, Ordering::AcqRel)
    }

    fn remote(&self, url: &str) -> Lookup {
        let mut entries = self.remote.lock().unwrap_or_else(|err| err.into_inner());
        match entries.get(url) {
            Some(Entry::Ready(picture)) => return Lookup::Ready(Arc::clone(picture)),
            Some(Entry::Pending) => return Lookup::Pending,
            Some(Entry::Failed) => return Lookup::Missing,
            None => {}
        }
        entries.insert(url.to_owned(), Entry::Pending);
        drop(entries);

        let remote = Arc::clone(&self.remote);
        let updated = Arc::clone(&self.updated);
        let url = url.to_owned();
        thread::spawn(move || {
            let entry = download(&url)
                .and_then(|bytes| decode(&bytes))
                .map_or(Entry::Failed, |picture| Entry::Ready(Arc::new(picture)));
            remote
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .insert(url, entry);
            updated.store(true, Ordering::Release);
        });
        Lookup::Pending
    }
}

fn download(url: &str) -> Option<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--fail",
            "--location",
            "--max-time",
            "10",
            "--max-filesize",
            "25000000",
            "--url",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

fn decode(bytes: &[u8]) -> Option<Picture> {
    if looks_like_svg(bytes) {
        return rasterize_svg(bytes);
    }
    let pixels = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = pixels.dimensions();
    Some(Picture {
        pixels,
        width: width as f32,
        height: height as f32,
    })
}

fn decode_data_url(data: &str) -> Option<Picture> {
    use base64::Engine as _;
    let (meta, payload) = data.split_once(',')?;
    let bytes = if meta.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok()?
    } else {
        crate::nav::percent_decode(payload).into_bytes()
    };
    decode(&bytes)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<') && head.contains("<svg")
}

fn rasterize_svg(bytes: &[u8]) -> Option<Picture> {
    let options = usvg::Options {
        fontdb: svg_fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).ok()?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    let scale = SVG_OVERSAMPLE.min(MAX_SVG_PIXELS / width.max(height).max(1.0));
    let pixel_w = (width * scale).ceil().max(1.0) as u32;
    let pixel_h = (height * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(pixel_w, pixel_h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut raw = Vec::with_capacity((pixel_w * pixel_h * 4) as usize);
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        raw.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    Some(Picture {
        pixels: RgbaImage::from_raw(pixel_w, pixel_h, raw)?,
        width,
        height,
    })
}

/// Badge SVGs carry their label as `<text>`, so they need system fonts.
fn svg_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    Arc::clone(FONTS.get_or_init(|| {
        let mut fonts = usvg::fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BADGE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="20"><rect width="80" height="20" fill="#4c1"/><text x="10" y="14" font-family="Verdana" font-size="11" fill="#fff">ok</text></svg>"##;

    #[test]
    fn svg_keeps_its_css_size_and_is_painted() {
        let picture = decode(BADGE.as_bytes()).expect("svg");
        assert_eq!((picture.width, picture.height), (80.0, 20.0));
        assert_eq!(picture.pixels.width(), 320);
        let green = picture.pixels.get_pixel(2, 2);
        assert_eq!(green.0[3], 255);
        assert!(green.0[1] > green.0[0]);
    }

    #[test]
    fn data_urls_decode_without_the_network() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(BADGE);
        let store = ImageStore::default();
        let lookup = store.get(
            &format!("data:image/svg+xml;base64,{encoded}"),
            Path::new("."),
        );
        assert!(matches!(lookup, Lookup::Ready(picture) if picture.width == 80.0));
    }

    #[test]
    fn a_missing_local_file_is_missing_not_pending() {
        let store = ImageStore::default();
        assert!(matches!(
            store.get("does-not-exist.png", Path::new(".")),
            Lookup::Missing
        ));
    }
}
