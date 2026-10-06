//! Draws a terminal screen as a PNG. The screen comes from tmux with its colour codes; the `vt100` crate (already used for the built-in terminal)
//! turns that into a grid of cells, and each cell is drawn with fonts that are part of this program, so the picture looks the same on every machine
//! and never depends on which fonts happen to be installed. Credentials are removed from the text before it is drawn.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use std::sync::LazyLock;

static MONO: LazyLock<FontRef<'static>> = LazyLock::new(|| {
    FontRef::try_from_slice(include_bytes!("../../assets/fonts/DejaVuSansMono.ttf"))
        .expect("bundled font")
});
static BOLD: LazyLock<FontRef<'static>> = LazyLock::new(|| {
    FontRef::try_from_slice(include_bytes!("../../assets/fonts/DejaVuSansMono-Bold.ttf"))
        .expect("bundled font")
});
/// For symbols the monospace font does not have.
static WIDE: LazyLock<FontRef<'static>> = LazyLock::new(|| {
    FontRef::try_from_slice(include_bytes!("../../assets/fonts/DejaVuSans.ttf"))
        .expect("bundled font")
});

const SIZE: f32 = 16.0;
const FG: [u8; 3] = [204, 204, 204];
const BG: [u8; 3] = [24, 24, 24];
/// The 16 standard colours.
const BASIC: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 49, 49],
    [13, 188, 121],
    [229, 229, 16],
    [36, 114, 200],
    [188, 63, 188],
    [17, 168, 205],
    [229, 229, 229],
    [102, 102, 102],
    [241, 76, 76],
    [35, 209, 139],
    [245, 245, 67],
    [59, 142, 234],
    [214, 112, 214],
    [41, 184, 219],
    [255, 255, 255],
];

fn colour(c: vt100::Color, default: [u8; 3]) -> [u8; 3] {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        vt100::Color::Idx(i) if i < 16 => BASIC[i as usize],
        vt100::Color::Idx(i) if i >= 232 => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
        vt100::Color::Idx(i) => {
            let n = i - 16;
            let level = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            [level(n / 36), level(n / 6 % 6), level(n % 6)]
        }
    }
}

/// One drawn cell: what it shows and how.
#[derive(Clone)]
struct Cell {
    ch: char,
    fg: [u8; 3],
    bg: [u8; 3],
    bold: bool,
    /// Covers this cell and the next one (a wide character).
    wide: bool,
    /// The right half of a wide character: nothing to draw.
    skip: bool,
}

/// The cells of the screen, with every row that held a credential redrawn from its cleaned text and the plain colours.
fn cells(ansi: &str, rows: u16, cols: u16) -> Vec<Vec<Cell>> {
    let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 0);
    // tmux separates rows with a bare newline.
    parser.process(ansi.replace('\n', "\r\n").as_bytes());
    let screen = parser.screen();
    let mut out = Vec::new();
    for r in 0..rows {
        let row: Vec<Cell> = (0..cols)
            .map(|c| match screen.cell(r, c) {
                Some(x) => {
                    let (mut fg, mut bg) = (colour(x.fgcolor(), FG), colour(x.bgcolor(), BG));
                    if x.inverse() {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    Cell {
                        ch: x.contents().chars().next().unwrap_or(' '),
                        fg,
                        bg,
                        bold: x.bold(),
                        wide: x.is_wide(),
                        skip: x.is_wide_continuation(),
                    }
                }
                None => Cell {
                    ch: ' ',
                    fg: FG,
                    bg: BG,
                    bold: false,
                    wide: false,
                    skip: false,
                },
            })
            .collect();
        out.push(row);
    }
    // The screen is cleaned as a whole: a credential can run over the end of a row (a full row goes on in the next one) or over several rows
    // (a private key), and a row on its own would show the rest of it.
    let mut text = String::new();
    for (r, row) in out.iter().enumerate() {
        let line: String = row.iter().map(|c| c.ch).collect();
        if r + 1 < out.len() && row.last().is_some_and(|c| c.ch != ' ') {
            text.push_str(&line);
        } else {
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    let clean = crate::security::redact::redact(&text);
    if clean.found.is_empty() {
        return out;
    }
    // Something was found: every character is replaced by the cleaned text, laid out again in rows (the colours stay where they were).
    let mut lines: Vec<Vec<char>> = Vec::new();
    for l in clean.text.split('\n') {
        let chars: Vec<char> = l.chars().collect();
        if chars.is_empty() {
            lines.push(chars);
        } else {
            lines.extend(chars.chunks(cols.max(1) as usize).map(<[char]>::to_vec));
        }
    }
    let styles = out.clone();
    for (r, row) in out.iter_mut().enumerate() {
        for (i, cell) in row.iter_mut().enumerate() {
            *cell = Cell {
                ch: lines.get(r).and_then(|l| l.get(i)).copied().unwrap_or(' '),
                wide: false,
                skip: false,
                ..styles[r][i].clone()
            };
        }
    }
    out
}

fn blend(px: &mut [u8], at: usize, fg: [u8; 3], cover: f32) {
    for k in 0..3 {
        let old = px[at + k] as f32;
        px[at + k] = (old + (fg[k] as f32 - old) * cover.clamp(0.0, 1.0)) as u8;
    }
}

/// Draws one character into the picture, at the cell whose top-left pixel is (x0, y0) and which is `w` pixels wide.
#[allow(clippy::too_many_arguments)]
fn glyph(
    px: &mut [u8],
    stride: usize,
    (x0, y0): (usize, usize),
    w: usize,
    ch: char,
    bold: bool,
    fg: [u8; 3],
    base: f32,
) {
    if ch == ' ' || ch == '\0' {
        return;
    }
    // Two marks Claude Code draws that no free font has: their nearest look-alikes.
    let ch = match ch {
        '⎿' => '└',
        '⏺' => '●',
        c => c,
    };
    let main: &FontRef = if bold { &BOLD } else { &MONO };
    let font: &FontRef = if main.glyph_id(ch).0 != 0 {
        main
    } else if WIDE.glyph_id(ch).0 != 0 {
        &WIDE
    } else {
        // No font has it (emoji, Chinese, Japanese): an empty box, so it is clear something is there.
        for i in 0..w.saturating_sub(2) {
            for y in [y0 + 3, y0 + base as usize] {
                blend(px, (y * stride + x0 + 1 + i) * 3, fg, 0.6);
            }
        }
        return;
    };
    let scaled = font.as_scaled(PxScale::from(SIZE));
    let g = scaled.scaled_glyph(ch);
    // Centred in its cell when the glyph is narrower than the cell, as symbols from the fallback font usually are.
    let advance = scaled.h_advance(g.id);
    let shift = ((w as f32 - advance) / 2.0).max(0.0);
    let mut g = g;
    g.position = ab_glyph::point(x0 as f32 + shift, y0 as f32 + base);
    if let Some(o) = font.outline_glyph(g) {
        let b = o.px_bounds();
        o.draw(|x, y, c| {
            let (gx, gy) = (b.min.x as i64 + x as i64, b.min.y as i64 + y as i64);
            if gx >= 0 && gy >= 0 && (gx as usize) < stride {
                let at = (gy as usize * stride + gx as usize) * 3;
                if at + 2 < px.len() {
                    blend(px, at, fg, c);
                }
            }
        });
    }
}

/// The picture at half its width and height.
fn halve(px: &[u8], w: usize, h: usize) -> (usize, usize, Vec<u8>) {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; nw * nh * 3];
    for y in 0..nh {
        for x in 0..nw {
            for k in 0..3 {
                let at = |dx: usize, dy: usize| px[((2 * y + dy) * w + 2 * x + dx) * 3 + k] as u32;
                out[(y * nw + x) * 3 + k] = ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / 4) as u8;
            }
        }
    }
    (nw, nh, out)
}

/// The screen as a PNG picture, or None if it cannot be drawn.
pub fn render(ansi: &str, rows: u16, cols: u16) -> Option<Vec<u8>> {
    if rows == 0 || cols == 0 || rows > 700 || cols > 400 {
        return None;
    }
    let scaled = MONO.as_scaled(PxScale::from(SIZE));
    let cw = scaled.h_advance(MONO.glyph_id('M')).round().max(1.0) as usize;
    let ch = (scaled.ascent() - scaled.descent()).ceil() as usize + 2;
    let base = scaled.ascent() + 1.0;
    let grid = cells(ansi, rows, cols);
    let (width, height) = (cw * cols as usize + 16, ch * rows as usize + 16);
    let mut px = vec![0u8; width * height * 3];
    for p in px.as_chunks_mut::<3>().0 {
        *p = BG;
    }
    for (r, row) in grid.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell.skip {
                continue;
            }
            let w = if cell.wide { cw * 2 } else { cw };
            let (x0, y0) = (8 + c * cw, 8 + r * ch);
            if cell.bg != BG {
                for y in y0..y0 + ch {
                    for x in x0..(x0 + w).min(width) {
                        let at = (y * width + x) * 3;
                        px[at..at + 3].copy_from_slice(&cell.bg);
                    }
                }
            }
            glyph(
                &mut px,
                width,
                (x0, y0),
                w,
                cell.ch,
                cell.bold,
                cell.fg,
                base,
            );
        }
    }
    // Half the size (each 2 x 2 block of pixels averaged into one), so the picture is small in the chat and in the file.
    let (width, height, px) = halve(&px, width, height);
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().ok()?.write_image_data(&px).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        (info.width, info.height, buf)
    }

    fn pixel(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let at = ((y * w + x) * 3) as usize;
        [buf[at], buf[at + 1], buf[at + 2]]
    }

    #[test]
    fn a_coloured_screen_becomes_a_png_of_the_right_size_with_its_colours() {
        let ansi = "plain\n\x1b[41m  red  \x1b[0m\n\x1b[7minverse\x1b[0m ❯ ✻ │ ─";
        let png = render(ansi, 5, 20).expect("drawn");
        assert_eq!(&png[1..4], b"PNG");
        let (w, h, buf) = decode(&png);
        // Half of the full-size picture: 20 columns of 10 pixels and 5 rows of 21, with 8 pixels of margin all round.
        assert!(w > 60 && w < 120 && h > 30 && h < 80, "{w} x {h}");
        // The red background of the second row is somewhere in the picture, and the plain background is the rest.
        let reds = buf
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| *p == &BASIC[1])
            .count();
        assert!(reds > 25, "red cells were not drawn: {reds}");
        assert_eq!(pixel(&buf, w, w - 1, h - 1), BG);
        // Something light was drawn for the text.
        assert!(
            buf.as_chunks::<3>()
                .0
                .iter()
                .any(|p| p[0] > 110 && p[1] > 110 && p[2] > 110)
        );
    }

    #[test]
    fn symbols_the_agents_programs_use_are_in_the_bundled_fonts() {
        for c in ['❯', '✻', '└', '●', '│', '─', '╭', '…', '→', '✓'] {
            assert!(
                MONO.glyph_id(c).0 != 0 || WIDE.glyph_id(c).0 != 0,
                "{c} is in neither bundled font"
            );
        }
    }

    #[test]
    fn a_credential_on_the_screen_is_not_in_the_picture_text() {
        let secret = "export API_KEY=abcdefghijklmnop1234";
        let grid = cells(secret, 2, 60);
        let shown: String = grid[0].iter().map(|c| c.ch).collect();
        assert!(
            shown.contains("[redacted") && !shown.contains("abcdefghijklmnop1234"),
            "{shown}"
        );
    }

    #[test]
    fn nonsense_sizes_and_empty_screens_do_not_panic() {
        assert!(render("", 0, 0).is_none());
        assert!(render("", 1000, 1000).is_none());
        assert!(render("", 3, 10).is_some());
        assert!(render("\x1b[38;2;1;2;3m\x1b[?1049h\x1b[999;999H日本語 😀", 4, 20).is_some());
    }

    fn shown(ansi: &str, rows: u16, cols: u16) -> String {
        cells(ansi, rows, cols)
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_credential_that_wraps_or_spans_rows_or_hides_in_escapes_is_gone() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for (s, left_over) in [
            (format!("{{\n  \"token\": \"{hex}\"\n}}"), "89abcdef01"),
            ("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnop12345678".into(), "bcdefghijk"),
            ("-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nSECRETBODYLINE2xxxxxxxxxxxxxxxx\n-----END PRIVATE KEY-----".into(), "SECRETBODY"),
            ("sk-ABCDEFGHIJ\x1b[31mKLMNOPQRSTUVWXYZabcdef".into(), "KLMNOPQRST"),
            ("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123".into(), "ijklmnop"),
            ("日本語 ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD".into(), "tuvwxyz01"),
        ] {
            let out = shown(&s, 8, 30);
            assert!(!out.contains(left_over), "{out}");
        }
    }
}
