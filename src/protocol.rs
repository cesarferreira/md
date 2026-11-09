//! Kitty graphics protocol commands and terminal replies.

const CHUNK: usize = 4096;

pub fn probe_query() -> String {
    let mut query = String::from(
        "\u{1b}_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\u{1b}\\\u{1b}[c\u{1b}[16t\u{1b}[14t\u{1b}]11;?\u{1b}\\",
    );
    for index in 0..16 {
        query.push_str(&format!("\u{1b}]4;{index};?\u{1b}\\"));
    }
    query
}

pub fn supports_graphics(response: &str) -> bool {
    response.contains("_G") && response.contains("OK")
}

pub fn transmit_png(id: u32, png: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    if encoded.is_empty() {
        return command(&format!("a=t,f=100,t=d,i={id},q=2,m=0"), "");
    }

    let mut out = String::new();
    let mut offset = 0;
    let mut first = true;
    while offset < encoded.len() {
        let end = (offset + CHUNK).min(encoded.len());
        let more = u8::from(end < encoded.len());
        let chunk = &encoded[offset..end];
        if first {
            out.push_str(&command(
                &format!("a=t,f=100,t=d,i={id},q=2,m={more}"),
                chunk,
            ));
            first = false;
        } else {
            out.push_str(&command(&format!("q=2,m={more}"), chunk));
        }
        offset = end;
    }
    out
}

pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub fn place_image(id: u32, placement: u32, source: SourceRect, cols: u32, rows: u32) -> String {
    // `p` replaces an existing placement of this image, so scrolling can move
    // the source rectangle without transmitting the pixels again.
    command(
        &format!(
            "a=p,i={id},p={placement},x={},y={},w={},h={},c={cols},r={rows},C=1,q=2",
            source.x, source.y, source.w, source.h
        ),
        "",
    )
}

/// Delete one image and its stored data.
pub fn delete_image(id: u32) -> String {
    command(&format!("a=d,d=I,i={id},q=2"), "")
}

/// Cell size from a `CSI 16 t` reply. Returns `(width, height)` in pixels.
pub fn parse_cell_size(response: &str) -> Option<(u32, u32)> {
    parse_csi_pair(response, "\u{1b}[6;").map(|(height, width)| (width, height))
}

/// Window size from a `CSI 14 t` reply. Returns `(width, height)` in pixels.
pub fn parse_window_pixels(response: &str) -> Option<(u32, u32)> {
    parse_csi_pair(response, "\u{1b}[4;").map(|(height, width)| (width, height))
}

pub fn parse_background(response: &str) -> Option<(u8, u8, u8)> {
    let start = response.find("]11;")?;
    rgb_at(&response[start..])
}

/// The 16 ANSI colors from `OSC 4` replies. Missing slots stay `None`.
pub fn parse_palette(response: &str) -> [Option<(u8, u8, u8)>; 16] {
    let mut colors = [None; 16];
    let mut rest = response;
    while let Some(index) = rest.find("]4;") {
        let after = &rest[index + 3..];
        let Some((slot, tail)) = after.split_once(';') else {
            rest = &rest[index + 3..];
            continue;
        };
        if let Ok(slot) = slot.parse::<usize>()
            && slot < 16
        {
            colors[slot] = rgb_at(tail);
        }
        rest = &rest[index + 3..];
    }
    colors
}

fn rgb_at(response: &str) -> Option<(u8, u8, u8)> {
    let rest = response.split("rgb:").nth(1)?;
    let mut parts = rest.split(['/', '\\', '\u{1b}', '\u{7}']);
    let red = scale_hex(parts.next()?)?;
    let green = scale_hex(parts.next()?)?;
    let blue = scale_hex(parts.next()?)?;
    Some((red, green, blue))
}

pub fn background_is_light(rgb: (u8, u8, u8)) -> bool {
    let (red, green, blue) = rgb;
    0.2126 * f32::from(red) + 0.7152 * f32::from(green) + 0.0722 * f32::from(blue) > 140.0
}

fn command(control: &str, payload: &str) -> String {
    format!("\u{1b}_G{control};{payload}\u{1b}\\")
}

fn parse_csi_pair(response: &str, marker: &str) -> Option<(u32, u32)> {
    let mut rest = response;
    while let Some(index) = rest.find(marker) {
        let after = &rest[index + marker.len()..];
        if let Some(pair) = pair_before_t(after) {
            return Some(pair);
        }
        rest = &rest[index + marker.len()..];
    }
    None
}

fn pair_before_t(value: &str) -> Option<(u32, u32)> {
    let (first, tail) = value.split_once(';')?;
    let end = tail.find('t')?;
    let second = &tail[..end];
    if !second.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((first.parse().ok()?, second.parse().ok()?))
}

fn scale_hex(value: &str) -> Option<u8> {
    let digits = value
        .chars()
        .take_while(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let parsed = u32::from_str_radix(&digits, 16).ok()?;
    let max = (1u32 << (digits.len() * 4)) - 1;
    Some(u8::try_from(parsed * 255 / max.max(1)).unwrap_or(255))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_ok_is_support() {
        let sample = "\u{1b}_Gi=31;OK\u{1b}\\\u{1b}[?62;c";
        assert!(supports_graphics(sample));
    }

    #[test]
    fn device_attributes_alone_are_not_support() {
        assert!(!supports_graphics("\u{1b}[?62;c"));
    }

    #[test]
    fn transmit_chunks_a_large_payload_and_finishes_the_last_chunk() {
        let png = vec![7u8; 4000];
        let encoded = transmit_png(7, &png);
        let commands = split_commands(&encoded);
        assert!(commands.len() > 1);
        assert!(commands[0].starts_with("\u{1b}_Ga=t,f=100,t=d,i=7,q=2,m=1;"));
        assert!(commands.last().unwrap().contains("m=0;"));
        assert!(commands.iter().all(|command| command.ends_with("\u{1b}\\")));
    }

    #[test]
    fn place_uses_the_source_rectangle() {
        assert_eq!(
            place_image(
                7,
                1,
                SourceRect {
                    x: 0,
                    y: 40,
                    w: 800,
                    h: 600,
                },
                80,
                30,
            ),
            "\u{1b}_Ga=p,i=7,p=1,x=0,y=40,w=800,h=600,c=80,r=30,C=1,q=2;\u{1b}\\"
        );
    }

    #[test]
    fn delete_frees_image_data() {
        assert_eq!(delete_image(4), "\u{1b}_Ga=d,d=I,i=4,q=2;\u{1b}\\");
    }

    #[test]
    fn cell_size_reply_is_height_then_width() {
        assert_eq!(parse_cell_size("\u{1b}[6;22;11t"), Some((11, 22)));
    }

    #[test]
    fn window_pixel_reply_is_height_then_width() {
        assert_eq!(parse_window_pixels("\u{1b}[4;1200;800t"), Some((800, 1200)));
    }

    #[test]
    fn background_rgb_scales_wide_hex() {
        assert_eq!(
            parse_background("\u{1b}]11;rgb:ffff/8080/0000\u{1b}\\"),
            Some((255, 128, 0))
        );
        assert!(!background_is_light((0, 0, 0)));
        assert!(background_is_light((255, 255, 255)));
    }

    #[test]
    fn palette_replies_do_not_steal_the_background() {
        let response = "\u{1b}]4;1;rgb:ffff/0000/0000\u{1b}\\\u{1b}]11;rgb:8080/8080/8080\u{1b}\\\u{1b}]4;4;rgb:0000/0000/ffff\u{1b}\\";
        assert_eq!(parse_background(response), Some((128, 128, 128)));
        let palette = parse_palette(response);
        assert_eq!(palette[1], Some((255, 0, 0)));
        assert_eq!(palette[4], Some((0, 0, 255)));
        assert!(palette[0].is_none());
    }

    fn split_commands(encoded: &str) -> Vec<&str> {
        encoded.split_inclusive("\u{1b}\\").collect()
    }
}
