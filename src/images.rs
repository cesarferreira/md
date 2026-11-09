//! Image loading. Local files and data URLs load on demand. Remote images come
//! from a disk cache when one exists, and otherwise from one shared HTTP client
//! so badges on the same host share a connection.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

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

#[derive(Clone)]
pub struct ImageStore {
    remote: Arc<Mutex<HashMap<String, Entry>>>,
    updated: Arc<AtomicBool>,
    cache_dir: Arc<PathBuf>,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            remote: Arc::new(Mutex::new(HashMap::new())),
            updated: Arc::new(AtomicBool::new(false)),
            cache_dir: Arc::new(default_cache_dir()),
        }
    }
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

    /// True while a remote image is still on the network.
    pub fn pending(&self) -> bool {
        self.remote
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .any(|entry| matches!(entry, Entry::Pending))
    }

    fn remote(&self, url: &str) -> Lookup {
        if let Some(lookup) = self.cached_lookup(url) {
            return lookup;
        }
        // A fresh copy is already on disk: decode it now so the first frame
        // has the picture, and refresh it quietly once it gets old.
        if let Some((bytes, fresh)) = read_cache(&self.cache_dir, url)
            && let Some(picture) = decode(&bytes)
        {
            let picture = Arc::new(picture);
            self.remember(url, Entry::Ready(Arc::clone(&picture)));
            if !fresh {
                self.spawn_fetch(url);
            }
            return Lookup::Ready(picture);
        }
        if self.mark_pending(url) {
            self.spawn_fetch(url);
        }
        self.cached_lookup(url).unwrap_or(Lookup::Pending)
    }

    fn cached_lookup(&self, url: &str) -> Option<Lookup> {
        let entries = self.remote.lock().unwrap_or_else(|err| err.into_inner());
        match entries.get(url) {
            Some(Entry::Ready(picture)) => Some(Lookup::Ready(Arc::clone(picture))),
            Some(Entry::Pending) => Some(Lookup::Pending),
            Some(Entry::Failed) => Some(Lookup::Missing),
            None => None,
        }
    }

    fn remember(&self, url: &str, entry: Entry) {
        self.remote
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(url.to_owned(), entry);
    }

    /// Returns true when this call is the one that should start the download.
    fn mark_pending(&self, url: &str) -> bool {
        let mut entries = self.remote.lock().unwrap_or_else(|err| err.into_inner());
        if entries.contains_key(url) {
            return false;
        }
        entries.insert(url.to_owned(), Entry::Pending);
        true
    }

    fn spawn_fetch(&self, url: &str) {
        let remote = Arc::clone(&self.remote);
        let updated = Arc::clone(&self.updated);
        let cache_dir = Arc::clone(&self.cache_dir);
        let url = url.to_owned();
        thread::spawn(move || {
            let Some(bytes) = download(&url) else {
                let mut entries = remote.lock().unwrap_or_else(|err| err.into_inner());
                if matches!(entries.get(&url), Some(Entry::Pending) | None) {
                    entries.insert(url, Entry::Failed);
                    updated.store(true, Ordering::Release);
                }
                return;
            };
            write_cache(&cache_dir, &url, &bytes);
            let Some(picture) = decode(&bytes) else {
                let mut entries = remote.lock().unwrap_or_else(|err| err.into_inner());
                if matches!(entries.get(&url), Some(Entry::Pending) | None) {
                    entries.insert(url, Entry::Failed);
                    updated.store(true, Ordering::Release);
                }
                return;
            };
            let picture = Arc::new(picture);
            let mut entries = remote.lock().unwrap_or_else(|err| err.into_inner());
            let changed = !matches!(
                entries.get(&url),
                Some(Entry::Ready(old)) if old.pixels.as_raw() == picture.pixels.as_raw()
            );
            entries.insert(url, Entry::Ready(picture));
            drop(entries);
            if changed {
                updated.store(true, Ordering::Release);
            }
        });
    }
}

/// Show a cached image immediately, and fetch a new copy after this age.
const CACHE_FRESH: Duration = Duration::from_secs(6 * 60 * 60);

fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("md").join("images")
}

fn cache_path(dir: &Path, url: &str) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    dir.join(format!("{hash:016x}"))
}

fn read_cache(dir: &Path, url: &str) -> Option<(Vec<u8>, bool)> {
    let path = cache_path(dir, url);
    let bytes = fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let fresh = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < CACHE_FRESH);
    Some((bytes, fresh))
}

fn write_cache(dir: &Path, url: &str, bytes: &[u8]) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = cache_path(dir, url);
    let temporary = path.with_extension("part");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(&temporary, &path);
    }
}

fn download(url: &str) -> Option<Vec<u8>> {
    let response = client().get(url).call().ok()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(25_000_000)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

fn client() -> &'static ureq::Agent {
    static CLIENT: OnceLock<ureq::Agent> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(8))
            .user_agent("md")
            .build()
    })
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

/// Badge SVGs name a sans face such as Verdana. Load those files directly.
/// Scanning every installed font is what made the first badges take seconds.
fn svg_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    Arc::clone(FONTS.get_or_init(|| {
        let mut fonts = usvg::fontdb::Database::new();
        for path in [
            "/System/Library/Fonts/Supplemental/Verdana.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ] {
            let _ = fonts.load_font_file(path);
        }
        if fonts.is_empty() {
            fonts.load_system_fonts();
        }
        let fallback = fonts
            .faces()
            .find_map(|face| face.families.first().map(|(name, _)| name.clone()));
        if let Some(name) = fallback {
            fonts.set_sans_serif_family(name.clone());
            fonts.set_serif_family(name);
        }
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
    fn a_cached_remote_image_is_ready_without_a_fetch() {
        let dir = std::env::temp_dir().join(format!("md-images-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let url = "https://img.shields.io/badge/license-MIT-green";
        fs::write(cache_path(&dir, url), BADGE.as_bytes()).unwrap();
        let store = ImageStore {
            remote: Arc::new(Mutex::new(HashMap::new())),
            updated: Arc::new(AtomicBool::new(false)),
            cache_dir: Arc::new(dir.clone()),
        };
        let lookup = store.get(url, Path::new("."));
        assert!(matches!(lookup, Lookup::Ready(picture) if picture.width == 80.0));
        assert!(!store.pending());
        let _ = fs::remove_dir_all(&dir);
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
