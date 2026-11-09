//! Where a clicked link goes.

use std::path::{Path, PathBuf};

use crate::document::{is_markdown_path, slug};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
    External(String),
    Scroll(String),
    File {
        path: PathBuf,
        anchor: Option<String>,
        markdown: bool,
    },
}

pub fn resolve_link(dest: &str, base_dir: &Path) -> Option<Nav> {
    let dest = dest.trim();
    if dest.is_empty() {
        return None;
    }
    if let Some(anchor) = dest.strip_prefix('#') {
        let anchor = slug(anchor);
        if anchor.is_empty() {
            return None;
        }
        return Some(Nav::Scroll(anchor));
    }
    if dest.starts_with("mailto:") || dest.starts_with("http://") || dest.starts_with("https://") {
        return Some(Nav::External(dest.to_owned()));
    }

    let (path, anchor) = dest.split_once('#').map_or((dest, None), |(path, anchor)| {
        (path, Some(slug(anchor)).filter(|value| !value.is_empty()))
    });
    let path = percent_decode(path);
    if path.is_empty() {
        return anchor.map(Nav::Scroll);
    }
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    let markdown = is_markdown_path(&path);
    Some(Nav::File {
        path,
        anchor,
        markdown,
    })
}

pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_and_mail_links_leave_the_preview() {
        assert_eq!(
            resolve_link("https://example.com/a", Path::new("/tmp")),
            Some(Nav::External("https://example.com/a".into()))
        );
        assert_eq!(
            resolve_link("mailto:a@example.com", Path::new("/tmp")),
            Some(Nav::External("mailto:a@example.com".into()))
        );
    }

    #[test]
    fn hash_links_scroll() {
        assert_eq!(
            resolve_link("#Hello World", Path::new(".")),
            Some(Nav::Scroll("hello-world".into()))
        );
    }

    #[test]
    fn relative_markdown_stays_in_the_preview() {
        assert_eq!(
            resolve_link("docs/guide.md#Setup", Path::new("/work")),
            Some(Nav::File {
                path: PathBuf::from("/work/docs/guide.md"),
                anchor: Some("setup".into()),
                markdown: true,
            })
        );
    }

    #[test]
    fn other_files_open_outside_the_preview() {
        assert_eq!(
            resolve_link("images/pic.png", Path::new("/work")),
            Some(Nav::File {
                path: PathBuf::from("/work/images/pic.png"),
                anchor: None,
                markdown: false,
            })
        );
    }
}
