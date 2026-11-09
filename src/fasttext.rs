//! Direct glyph placement for text that does not need a shaper.
//!
//! A long README is almost all Latin and monospace. Shaping every block with
//! HarfBuzz is what made those pages take a third of a second, and a resize
//! paid it again. This path measures advances and paints cached masks.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::page::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    face: u8,
    px: u16,
    glyph: u16,
}

pub struct Ink {
    pub x: i32,
    pub y: i32,
    pub key: GlyphKey,
    pub color: [u8; 4],
}

pub struct Spec<'a> {
    pub text: &'a str,
    pub mono: bool,
    pub bold: bool,
    pub italic: bool,
    pub size: f32,
    pub color: [u8; 4],
    pub link: Option<&'a str>,
    pub strike: bool,
    pub code: bool,
}

pub struct LayoutOpts {
    pub wrap: Option<f32>,
    pub center: bool,
    pub x: f32,
    pub y: f32,
}

pub struct Laid {
    pub width: f32,
    pub height: f32,
    pub inks: Vec<Ink>,
    pub pills: Vec<Rect>,
    pub strikes: Vec<Rect>,
    pub links: Vec<(String, Rect)>,
    pub atoms: Vec<(Rect, String)>,
}

struct Mask {
    width: u32,
    height: u32,
    coverage: Vec<u8>,
}

struct Placed {
    ch: char,
    face: u8,
    px: u16,
    glyph: u16,
    advance: f32,
    xmin: f32,
    ymin: f32,
    width: u32,
    height: u32,
    ascent: f32,
    line_h: f32,
    color: [u8; 4],
    link: Option<String>,
    strike: bool,
    code: bool,
    space: bool,
}

pub struct FastText {
    faces: [Option<Font>; 5],
    masks: HashMap<GlyphKey, Mask>,
    ready: bool,
}

impl FastText {
    pub fn new() -> Self {
        Self {
            faces: [None, None, None, None, None],
            masks: HashMap::new(),
            ready: false,
        }
    }

    pub fn mask(&self, key: GlyphKey) -> Option<(u32, u32, &[u8])> {
        let mask = self.masks.get(&key)?;
        Some((mask.width, mask.height, &mask.coverage))
    }

    pub fn layout(&mut self, specs: &[Spec<'_>], opts: LayoutOpts) -> Option<Laid> {
        self.ensure();
        self.faces[0].as_ref()?;
        let chars = self.place(specs)?;
        Some(self.compose(&chars, opts))
    }

    fn ensure(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        self.faces[0] = load(&[
            "/System/Library/Fonts/Supplemental/Verdana.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ]);
        self.faces[1] = load(&[
            "/System/Library/Fonts/Supplemental/Verdana Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        ]);
        self.faces[2] = load(&[
            "/System/Library/Fonts/Supplemental/Verdana Italic.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Italic.ttf",
        ]);
        self.faces[3] = load(&[
            "/System/Library/Fonts/Supplemental/Verdana Bold Italic.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-BoldOblique.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-BoldItalic.ttf",
        ]);
        self.faces[4] = load_indexed(&[
            ("/System/Library/Fonts/Menlo.ttc", 0),
            ("/System/Library/Fonts/Monaco.ttf", 0),
            ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0),
            (
                "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
                0,
            ),
        ]);
    }

    fn place(&mut self, specs: &[Spec<'_>]) -> Option<Vec<Placed>> {
        let mut out = Vec::new();
        for spec in specs {
            let face = self.face_index(spec);
            let px = (spec.size.max(1.0) * 2.0).round() / 2.0;
            let px_key = (px * 2.0).round() as u16;
            let (ascent, line_h) = {
                let font = self.faces[face].as_ref()?;
                let line = font.horizontal_line_metrics(px)?;
                let line_h = (line.ascent + line.descent.abs() + line.line_gap).max(1.0);
                (line.ascent, line_h)
            };
            for ch in spec.text.chars() {
                if ch == '\n' || ch == '\r' {
                    out.push(newline(spec));
                    continue;
                }
                if ch == '\t' || ch == ' ' || ch == '\u{00a0}' {
                    let space = {
                        let font = self.faces[face].as_ref()?;
                        font.metrics(' ', px).advance_width.max(px * 0.33)
                    };
                    let advance = if ch == '\t' { space * 4.0 } else { space };
                    out.push(gap(spec, advance, ascent, line_h));
                    continue;
                }
                let (glyph, xmin, ymin, width, height, advance) = {
                    let font = self.faces[face].as_ref()?;
                    let glyph = font.lookup_glyph_index(ch);
                    if glyph == 0 {
                        return None;
                    }
                    let metrics = font.metrics_indexed(glyph, px);
                    (
                        glyph,
                        metrics.xmin as f32,
                        metrics.ymin as f32,
                        metrics.width as u32,
                        metrics.height as u32,
                        metrics.advance_width,
                    )
                };
                self.raster(face, px, px_key, glyph);
                out.push(Placed {
                    ch,
                    face: face as u8,
                    px: px_key,
                    glyph,
                    advance,
                    xmin,
                    ymin,
                    width,
                    height,
                    ascent,
                    line_h,
                    color: spec.color,
                    link: spec.link.map(str::to_owned),
                    strike: spec.strike,
                    code: spec.code,
                    space: false,
                });
            }
        }
        Some(out)
    }

    fn face_index(&self, spec: &Spec<'_>) -> usize {
        let want = if spec.mono {
            4
        } else if spec.bold && spec.italic {
            3
        } else if spec.bold {
            1
        } else if spec.italic {
            2
        } else {
            0
        };
        if self.faces[want].is_some() { want } else { 0 }
    }

    fn raster(&mut self, face: usize, px: f32, px_key: u16, glyph: u16) {
        let key = GlyphKey {
            face: face as u8,
            px: px_key,
            glyph,
        };
        if self.masks.contains_key(&key) {
            return;
        }
        let Some((width, height, coverage)) = self.faces[face].as_ref().map(|font| {
            let (metrics, coverage) = font.rasterize_indexed(glyph, px);
            (metrics.width as u32, metrics.height as u32, coverage)
        }) else {
            return;
        };
        self.masks.insert(
            key,
            Mask {
                width,
                height,
                coverage,
            },
        );
    }

    fn compose(&self, chars: &[Placed], opts: LayoutOpts) -> Laid {
        let mut laid = Laid {
            width: 0.0,
            height: 0.0,
            inks: Vec::new(),
            pills: Vec::new(),
            strikes: Vec::new(),
            links: Vec::new(),
            atoms: Vec::new(),
        };
        let mut y = opts.y;
        let mut index = 0;
        while index < chars.len() {
            let start = index;
            while index < chars.len() && chars[index].ch != '\n' {
                index += 1;
            }
            let logical = start..index;
            let blank = chars.get(index).map(|ch| ch.line_h).unwrap_or(0.0);
            if index < chars.len() {
                index += 1;
            }
            if logical.is_empty() {
                y += blank.max(1.0);
                continue;
            }
            for visual in wrap_line(chars, logical, opts.wrap) {
                y = self.place_line(&mut laid, chars, visual, y, &opts);
            }
        }
        laid.height = (y - opts.y).max(0.0);
        laid
    }

    fn place_line(
        &self,
        laid: &mut Laid,
        chars: &[Placed],
        visual: std::ops::Range<usize>,
        y: f32,
        opts: &LayoutOpts,
    ) -> f32 {
        let slice = &chars[visual];
        if slice.is_empty() {
            return y;
        }
        let line_h = slice.iter().map(|ch| ch.line_h).fold(1.0_f32, f32::max);
        let ascent = slice.iter().map(|ch| ch.ascent).fold(0.0_f32, f32::max);
        let content: f32 = slice
            .iter()
            .rev()
            .skip_while(|ch| ch.space)
            .map(|ch| ch.advance)
            .sum();
        let shift = match opts.wrap {
            Some(width) if opts.center => ((width - content) / 2.0).max(0.0),
            _ => 0.0,
        };
        let mut pen = opts.x + shift;
        let atom_start = pen;
        let mut atom = String::new();
        let mut pill: Option<Rect> = None;
        let mut link: Option<(String, Rect)> = None;
        let baseline = y + if ascent > 0.0 { ascent } else { line_h * 0.8 };
        for ch in slice {
            if !ch.space && ch.width > 0 && ch.height > 0 {
                laid.inks.push(Ink {
                    x: (pen + ch.xmin).round() as i32,
                    y: (baseline - ch.ymin - ch.height as f32).round() as i32,
                    key: GlyphKey {
                        face: ch.face,
                        px: ch.px,
                        glyph: ch.glyph,
                    },
                    color: ch.color,
                });
            }
            let rect = Rect {
                x: pen,
                y,
                w: ch.advance.max(0.4),
                h: line_h,
            };
            if ch.code {
                pill = Some(match pill {
                    Some(prev) => prev.union(rect),
                    None => rect,
                });
            } else if let Some(prev) = pill.take() {
                laid.pills.push(prev);
            }
            if ch.strike {
                laid.strikes.push(Rect {
                    x: pen,
                    y: y + line_h * 0.55,
                    w: ch.advance.max(0.4),
                    h: 1.0,
                });
            }
            if let Some(url) = &ch.link {
                match &mut link {
                    Some((current, region)) if current == url => *region = region.union(rect),
                    other => {
                        if let Some(prev) = other.take() {
                            laid.links.push(prev);
                        }
                        link = Some((url.clone(), rect));
                    }
                }
            } else if let Some(prev) = link.take() {
                laid.links.push(prev);
            }
            atom.push(ch.ch);
            pen += ch.advance;
        }
        if let Some(prev) = pill {
            laid.pills.push(prev);
        }
        if let Some(prev) = link {
            laid.links.push(prev);
        }
        let text = atom.trim_end().to_owned();
        if !text.is_empty() {
            laid.atoms.push((
                Rect {
                    x: atom_start,
                    y,
                    w: (pen - atom_start).max(1.0),
                    h: line_h,
                },
                text,
            ));
        }
        laid.width = laid.width.max(pen - opts.x);
        y + line_h
    }
}

fn newline(spec: &Spec<'_>) -> Placed {
    Placed {
        ch: '\n',
        face: 0,
        px: 0,
        glyph: 0,
        advance: 0.0,
        xmin: 0.0,
        ymin: 0.0,
        width: 0,
        height: 0,
        ascent: 0.0,
        line_h: (spec.size.max(1.0) * 1.45).max(1.0),
        color: spec.color,
        link: None,
        strike: false,
        code: false,
        space: false,
    }
}

fn gap(spec: &Spec<'_>, advance: f32, ascent: f32, line_h: f32) -> Placed {
    Placed {
        ch: ' ',
        face: 0,
        px: 0,
        glyph: 0,
        advance,
        xmin: 0.0,
        ymin: 0.0,
        width: 0,
        height: 0,
        ascent,
        line_h,
        color: spec.color,
        link: spec.link.map(str::to_owned),
        strike: spec.strike,
        code: spec.code,
        space: true,
    }
}

fn wrap_line(
    chars: &[Placed],
    logical: std::ops::Range<usize>,
    wrap: Option<f32>,
) -> Vec<std::ops::Range<usize>> {
    let Some(limit) = wrap else {
        return vec![logical];
    };
    if logical.is_empty() {
        return vec![logical];
    }
    let mut lines = Vec::new();
    let mut start = logical.start;
    let mut x = 0.0_f32;
    let mut index = logical.start;
    while index < logical.end {
        let adv = chars[index].advance;
        if x > 0.0 && x + adv > limit && !chars[index].space {
            if let Some(brk) = (start..index).rfind(|&at| chars[at].space) {
                lines.push(start..brk);
                start = brk + 1;
                index = start;
                x = 0.0;
                continue;
            }
            if x > 0.0 {
                lines.push(start..index);
                start = index;
                x = 0.0;
                continue;
            }
        }
        x += adv;
        index += 1;
    }
    if start < logical.end || lines.is_empty() {
        lines.push(start..logical.end);
    }
    lines
}

fn load(paths: &[&str]) -> Option<Font> {
    load_indexed(&paths.iter().map(|path| (*path, 0)).collect::<Vec<_>>())
}

fn load_indexed(paths: &[(&str, u32)]) -> Option<Font> {
    for (path, index) in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let settings = FontSettings {
            collection_index: *index,
            ..FontSettings::default()
        };
        if let Ok(font) = Font::from_bytes(bytes, settings) {
            return Some(font);
        }
    }
    None
}
