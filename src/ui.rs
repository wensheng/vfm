//! Terminal rendering: a thin, dirty-row screen over raw crossterm.
//! All output accumulates in a frame buffer and is flushed once per loop pass.
//! Knows nothing about `App` — primitives only (types shared with `ansi.rs`).

use std::io::Write;
use unicode_width::UnicodeWidthChar;

// ---------------------------------------------------------------------------
// Styles (shared with ansi.rs)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Color {
    #[default]
    Default,
    /// SGR palette index: 0-15 map to the standard/bright SGR codes, >=16 to
    /// `38;5;n` (the 256-color palette the preview parser produces).
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Attrs {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Style {
    pub fn fg(c: Color) -> Self {
        Self { fg: c, ..Default::default() }
    }
    pub fn is_default(self) -> bool {
        self.fg == Color::Default
            && self.bg == Color::Default
            && self.attrs
                == Attrs {
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                }
    }
}

impl std::ops::BitOr for Style {
    type Output = Style;
    /// Combine: `base | overlay` — overlay's colors win; attrs OR together.
    fn bitor(self, o: Style) -> Style {
        Style {
            fg: if o.fg == Color::Default { self.fg } else { o.fg },
            bg: if o.bg == Color::Default { self.bg } else { o.bg },
            attrs: Attrs {
                bold: self.attrs.bold || o.attrs.bold,
                dim: self.attrs.dim || o.attrs.dim,
                italic: self.attrs.italic || o.attrs.italic,
                underline: self.attrs.underline || o.attrs.underline,
                reverse: self.attrs.reverse || o.attrs.reverse,
            },
        }
    }
}

/// One styled run of text.
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// A line of styled text (a preview output row, overlay content, ...).
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { spans: vec![Span { text: text.into(), style: Style::default() }] }
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self { spans: vec![Span { text: text.into(), style }] }
    }
    pub fn from_spans(spans: Vec<Span>) -> Self {
        Self { spans }
    }
    /// Unstyled text of the whole line.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum()
}

// ---------------------------------------------------------------------------
// Theme — the file-color mapping (config overrides individual fields)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Theme {
    pub dir: Style,
    pub link: Style,
    pub orphan: Style,
    pub exec: Style,
    pub hardlink: Style,
    pub missing: Style,
    pub selected: Style,
    pub cursor: Style,
    pub statusbar: Style,
    pub ctx_active: Style,
    pub ctx_inactive: Style,
    pub divider: Style,
    pub preview_meta: Style,
    pub error: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            dir: Style::fg(Color::Idx(4)),      // blue
            link: Style::fg(Color::Idx(6)),     // cyan
            orphan: Style::fg(Color::Idx(1)),   // red
            exec: Style::fg(Color::Idx(2)),     // green
            hardlink: Style::fg(Color::Idx(3)), // yellow
            missing: Style::fg(Color::Idx(1)),
            selected: Style { attrs: Attrs { reverse: true, ..Default::default() }, ..Default::default() },
            cursor: Style {
                attrs: Attrs { reverse: true, bold: true, ..Default::default() },
                ..Default::default()
            },
            statusbar: Style::default(),
            ctx_active: Style { attrs: Attrs { reverse: true, ..Default::default() }, ..Default::default() },
            ctx_inactive: Style::default(),
            divider: Style { attrs: Attrs { dim: true, ..Default::default() }, ..Default::default() },
            preview_meta: Style { attrs: Attrs { dim: true, ..Default::default() }, ..Default::default() },
            error: Style {
                fg: Color::Idx(1),
                attrs: Attrs { bold: true, ..Default::default() },
                ..Default::default()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

/// Owns the terminal output stream. In tests, `out` points at a `Vec<u8>` —
/// assertions read the frame buffer directly via [`Screen::frame`].
pub struct Screen {
    out: Box<dyn Write>,
    pub w: u16,
    pub h: u16,
    /// Frame accumulator: everything queued since the last flush.
    pub buf: Vec<u8>,
    last_style: Option<Style>,
    cursor_visible: bool,
    mouse: bool,
}

impl Screen {
    pub fn new(out: Box<dyn Write>, w: u16, h: u16) -> Self {
        Self {
            out,
            w,
            h,
            buf: Vec::with_capacity(16 * 1024),
            last_style: None,
            cursor_visible: true,
            mouse: false,
        }
    }

    pub fn size_of_stdout() -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    /// Raw mode + alternate screen + hidden cursor + mouse capture.
    pub fn enter(&mut self) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            self.out,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        crossterm::execute!(self.out, crossterm::event::EnableMouseCapture)?;
        self.mouse = true;
        self.cursor_visible = false;
        self.out.flush()
    }

    /// Undo everything `enter` did. Safe to call twice (panic hook + drop).
    pub fn leave(&mut self) {
        if self.mouse {
            let _ = crossterm::execute!(self.out, crossterm::event::DisableMouseCapture);
            self.mouse = false;
        }
        let _ = crossterm::execute!(
            self.out,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        self.cursor_visible = true;
        let _ = crossterm::terminal::disable_raw_mode();
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
    }

    // -- frame primitives ---------------------------------------------------

    /// Listing rows: rows 0 (contexts) and 1 (path) above, statusbar below.
    pub fn list_rows(&self) -> usize {
        (self.h as usize).saturating_sub(3)
    }

    pub fn move_to(&mut self, row: u16, col: u16) {
        let _ = write!(self.buf, "\x1b[{};{}H", row + 1, col + 1);
    }

    /// Queue a style change (emits SGR only when it differs from the last).
    pub fn set_style(&mut self, s: Style) {
        if self.last_style == Some(s) {
            return;
        }
        self.last_style = Some(s);
        if s.is_default() {
            self.buf.extend_from_slice(b"\x1b[0m");
            return;
        }
        let a = &s.attrs;
        self.buf.extend_from_slice(b"\x1b[0;");
        for (on, code) in [
            (a.bold, "1;"),
            (a.dim, "2;"),
            (a.italic, "3;"),
            (a.underline, "4;"),
            (a.reverse, "7;"),
        ] {
            if on {
                self.buf.extend_from_slice(code.as_bytes());
            }
        }
        emit_color(&mut self.buf, s.fg, true);
        emit_color(&mut self.buf, s.bg, false);
        // Every param ends in ';' — turn the last into the terminator. A
        // trailing empty param would read as 0 (reset) and undo everything.
        *self.buf.last_mut().expect("SGR prefix written above") = b'm';
    }

    /// Raw text; caller is responsible for styles and widths.
    pub fn write_str(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// Write `text` at (row, col) with `style`, truncated to `cols` with `…`.
    /// Returns the display width actually consumed.
    pub fn put_text(&mut self, row: u16, col: u16, text: &str, style: Style, cols: usize) -> usize {
        self.move_to(row, col);
        self.set_style(style);
        let (t, w) = crate::util::truncate_cols(text, cols);
        self.write_str(&t);
        w
    }

    /// Render a styled `Line` into `row`, padded to `width` cells with `pad_style`.
    /// Tabs expand to 4 spaces; overflow truncates with `…`.
    pub fn put_line(&mut self, row: u16, line: &Line, width: usize, pad_style: Style) {
        self.put_line_at(row, 0, line, width, pad_style);
    }

    /// `put_line` starting at column `col` (preview pane, overlays).
    pub fn put_line_at(&mut self, row: u16, col: u16, line: &Line, width: usize, pad_style: Style) {
        if width == 0 {
            return;
        }
        self.move_to(row, col);
        let mut used = 0usize;
        let mut truncated = false;
        'outer: for span in &line.spans {
            self.set_style(pad_style | span.style);
            let expanded = span.text.replace('\t', "    ");
            for c in expanded.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > width.saturating_sub(1) {
                    truncated = true; // keep one cell for the ellipsis
                    break 'outer;
                }
                used += cw;
                let mut tmp = [0u8; 4];
                self.buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
        if truncated {
            self.set_style(pad_style);
            self.write_str("…");
            used += 1;
        }
        if used < width {
            self.set_style(pad_style);
            self.write_str(&" ".repeat(width - used));
        }
    }

    /// Queue everything written so far to the terminal. One write per pass.
    pub fn flush(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.out.write_all(&self.buf);
            self.buf.clear();
        }
        let _ = self.out.flush();
        self.last_style = None;
    }

}

impl Drop for Screen {
    fn drop(&mut self) {
        // Panic paths are already restored via the hook; this covers clean drops.
        self.leave();
    }
}

fn emit_color(buf: &mut Vec<u8>, c: Color, fg: bool) {
    match c {
        Color::Default => {} // the leading `0` reset already selected defaults
        Color::Idx(n) if n < 8 => {
            let _ = write!(buf, "{};", if fg { 30 + n as u32 } else { 40 + n as u32 });
        }
        Color::Idx(n) => {
            if n < 16 {
                let _ = write!(
                    buf,
                    "{};",
                    if fg { 90 + n as u32 - 8 } else { 100 + n as u32 - 8 }
                );
            } else {
                let _ = write!(buf, "{};5;{n};", if fg { 38 } else { 48 });
            }
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(buf, "{};2;{r};{g};{b};", if fg { 38 } else { 48 });
        }
    }
}

/// Replace control characters the nnn way: tab -> 4 spaces, C0 -> ^X form.
pub fn sanitize(name: &str) -> String {
    if name.bytes().all(|b| b >= 0x20 && b != 0x7F) {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\x7f' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push((b'A' + (c as u8) - 1) as char);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_emission_minimal() {
        let mut s = Screen::new(Box::new(Vec::new()), 80, 24);
        s.set_style(Style::fg(Color::Idx(4)));
        s.set_style(Style::fg(Color::Idx(4))); // deduped
        assert_eq!(s.buf, b"\x1b[0;34m");
        s.set_style(Style::default());
        assert!(s.buf.ends_with(b"\x1b[0m"));
    }

    #[test]
    fn sanitize_controls() {
        assert_eq!(sanitize("plain"), "plain");
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("a\x01b"), "a^Ab");
        assert_eq!(sanitize("a\x7fb"), "a^?b");
    }

    #[test]
    fn widths() {
        assert_eq!(str_width("abc"), 3);
        assert_eq!(str_width("你好"), 4);
    }

    #[test]
    fn put_line_pads_and_truncates() {
        let mut s = Screen::new(Box::new(Vec::new()), 80, 24);
        s.put_line(0, &Line::plain("hi"), 5, Style::default());
        let out = String::from_utf8(s.buf.clone()).unwrap();
        assert!(out.contains("hi   "), "got: {out:?}");
        s.buf.clear();
        s.put_line(0, &Line::plain("hello world"), 8, Style::default());
        let out = String::from_utf8(s.buf.clone()).unwrap();
        assert!(out.contains("hello w…"), "got: {out:?}");
    }
}
