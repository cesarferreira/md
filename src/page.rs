//! Hit targets on a rendered page.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }

    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub kind: RegionKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RegionKind {
    Link(String),
    Anchor(String),
    /// Summary line of the `n`th `<details>` block.
    Toggle(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Atom {
    pub rect: Rect,
    pub text: String,
    pub line: u32,
}

pub fn hit_link(regions: &[Region], x: f32, y: f32) -> Option<&str> {
    regions.iter().rev().find_map(|region| {
        if region.rect.contains(x, y) {
            match &region.kind {
                RegionKind::Link(url) => Some(url.as_str()),
                RegionKind::Anchor(_) | RegionKind::Toggle(_) => None,
            }
        } else {
            None
        }
    })
}

pub fn hit_toggle(regions: &[Region], x: f32, y: f32) -> Option<usize> {
    regions.iter().rev().find_map(|region| match region.kind {
        RegionKind::Toggle(index) if region.rect.contains(x, y) => Some(index),
        _ => None,
    })
}

pub fn anchor_y(regions: &[Region], slug: &str) -> Option<f32> {
    regions.iter().find_map(|region| match &region.kind {
        RegionKind::Anchor(name) if name == slug => Some(region.rect.y),
        _ => None,
    })
}

/// A laid-out block, in page pixels, tagged with its source lines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub start_line: u32,
    pub end_line: u32,
    pub rect: Rect,
}

/// The smallest block that covers `line`. Nested blocks win over their parents.
pub fn tightest_frame(frames: &[Frame], line: u32) -> Option<&Frame> {
    frames
        .iter()
        .filter(|frame| frame.start_line <= line && line <= frame.end_line)
        .min_by_key(|frame| (frame.end_line - frame.start_line, frame.rect.h as u32))
}

/// Block to reveal for a changed source line.
///
/// A line that sits in a gap (a blank line between blocks) snaps to the next
/// block, or to the previous one when the change is past the end.
pub fn frame_for_change(frames: &[Frame], line: u32) -> Option<&Frame> {
    tightest_frame(frames, line)
        .or_else(|| {
            frames
                .iter()
                .filter(|frame| frame.start_line >= line)
                .min_by_key(|frame| (frame.start_line, frame.end_line - frame.start_line))
        })
        .or_else(|| {
            frames
                .iter()
                .filter(|frame| frame.end_line < line)
                .max_by_key(|frame| (frame.end_line, frame.start_line))
        })
}

/// Scroll offset that puts the changed block near the top of the viewport.
pub fn change_scroll_y(frames: &[Frame], line: u32) -> Option<f32> {
    frame_for_change(frames, line).map(|frame| (frame.rect.y - 28.0).max(0.0))
}

pub fn selected_text(atoms: &[Atom], selection: Rect) -> String {
    let mut hits: Vec<&Atom> = atoms
        .iter()
        .filter(|atom| atom.rect.intersects(selection) && !atom.text.is_empty())
        .collect();
    hits.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then(left.rect.x.total_cmp(&right.rect.x))
    });

    let mut out = String::new();
    let mut last_line = None;
    for atom in hits {
        if last_line.is_some_and(|line| line != atom.line) {
            out.push('\n');
        }
        last_line = Some(atom.line);
        out.push_str(&atom.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn topmost_link_wins_over_an_earlier_region() {
        let regions = vec![
            Region {
                rect: rect(0.0, 0.0, 40.0, 20.0),
                kind: RegionKind::Link("https://first.example".into()),
            },
            Region {
                rect: rect(10.0, 0.0, 20.0, 20.0),
                kind: RegionKind::Link("https://second.example".into()),
            },
        ];
        assert_eq!(
            hit_link(&regions, 15.0, 5.0),
            Some("https://second.example")
        );
        assert_eq!(hit_link(&regions, 5.0, 5.0), Some("https://first.example"));
        assert_eq!(hit_link(&regions, 80.0, 5.0), None);
    }

    #[test]
    fn anchors_are_not_clicks() {
        let regions = vec![Region {
            rect: rect(0.0, 10.0, 100.0, 20.0),
            kind: RegionKind::Anchor("intro".into()),
        }];
        assert_eq!(hit_link(&regions, 4.0, 12.0), None);
        assert_eq!(anchor_y(&regions, "intro"), Some(10.0));
    }

    #[test]
    fn selection_joins_atoms_and_breaks_lines() {
        let atoms = vec![
            Atom {
                rect: rect(0.0, 0.0, 20.0, 10.0),
                text: "hello ".into(),
                line: 0,
            },
            Atom {
                rect: rect(20.0, 0.0, 20.0, 10.0),
                text: "world".into(),
                line: 0,
            },
            Atom {
                rect: rect(0.0, 16.0, 30.0, 10.0),
                text: "next".into(),
                line: 1,
            },
        ];
        let text = selected_text(&atoms, rect(0.0, 0.0, 100.0, 40.0));
        assert_eq!(text, "hello world\nnext");
        assert_eq!(selected_text(&atoms, rect(0.0, 16.0, 10.0, 10.0)), "next");
    }

    #[test]
    fn the_nested_block_is_the_change_target() {
        let frames = vec![
            Frame {
                start_line: 4,
                end_line: 4,
                rect: rect(40.0, 180.0, 400.0, 24.0),
            },
            Frame {
                start_line: 1,
                end_line: 6,
                rect: rect(20.0, 40.0, 500.0, 200.0),
            },
        ];
        let frame = tightest_frame(&frames, 4).unwrap();
        assert_eq!(frame.rect.y, 180.0);
        assert_eq!(change_scroll_y(&frames, 4), Some(152.0));
        assert_eq!(change_scroll_y(&frames, 1), Some(12.0));
        assert_eq!(tightest_frame(&frames, 9), None);
        assert_eq!(frame_for_change(&frames, 9).unwrap().rect.y, 40.0);
    }
}
