//! Incremental ANSI → styled-lines parser for plugin preview output.
//! Scope: SGR (`CSI … m`) styling only. All other escape sequences (cursor
//! movement, OSC incl. OSC 8 hyperlinks, DCS/APC) are consumed and dropped —
//! their text payload is kept. Never panics on malformed input; a multi-byte
//! UTF-8 char split across chunk boundaries is carried across `feed()` calls.

use crate::ui::{Color, Line, Span, Style};

#[derive(Clone, Copy, PartialEq, Default)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Escape,
    /// Inside CSI, collecting parameter bytes until a final byte (0x40..=0x7E).
    Csi,
    /// Inside OSC/DCS/APC: until BEL or ST (ESC \).
    StringSeq,
}

#[derive(Default)]
pub struct AnsiFeeder {
    state: State,
    /// CSI parameter bytes accumulated so far.
    seq: Vec<u8>,
    style: Style,
    /// Text since the last newline.
    cur: String,
    /// Byte offset in `cur` where the pending (same-style) span began.
    span_start: usize,
    spans: Vec<Span>,
    lines: Vec<Line>,
    /// Continuation bytes still needed to complete a split UTF-8 char.
    utf8_need: usize,
    utf8_buf: Vec<u8>,
}

impl AnsiFeeder {
    pub fn new() -> Self {
        Self { state: State::Ground, ..Default::default() }
    }

    /// Feed one chunk of raw plugin stdout.
    pub fn feed(&mut self, mut data: &[u8]) {
        loop {
            // Complete a char that was split by a previous chunk boundary.
            if self.utf8_need > 0 {
                if data.len() < self.utf8_need {
                    self.utf8_buf.extend_from_slice(data);
                    return;
                }
                self.utf8_buf.extend_from_slice(&data[..self.utf8_need]);
                data = &data[self.utf8_need..];
                match std::str::from_utf8(&self.utf8_buf) {
                    Ok(s) => self.cur.push_str(s),
                    Err(_) => self.cur.push('\u{FFFD}'),
                }
                self.utf8_need = 0;
                self.utf8_buf.clear();
                continue;
            }
            let Some(&b) = data.first() else { return };
            match self.state {
                State::Ground => match b {
                    0x1B => {
                        self.flush_span();
                        self.state = State::Escape;
                        data = &data[1..];
                    }
                    b'\n' => {
                        self.end_line();
                        data = &data[1..];
                    }
                    b'\t' => {
                        self.cur.push('\t');
                        data = &data[1..];
                    }
                    // All other C0/C1 controls: dropped.
                    0x00..=0x1F | 0x7F => {
                        data = &data[1..];
                    }
                    _ => {
                        let len = utf8_len(b);
                        if len == 1 {
                            self.cur.push(b as char);
                            data = &data[1..];
                        } else if data.len() < len {
                            self.utf8_need = len - data.len(); // remaining bytes
                            self.utf8_buf = data.to_vec();
                            return;
                        } else {
                            match std::str::from_utf8(&data[..len]) {
                                Ok(s) => self.cur.push_str(s),
                                Err(_) => self.cur.push('\u{FFFD}'),
                            }
                            data = &data[len..];
                        }
                    }
                },
                State::Escape => {
                    data = &data[1..];
                    self.state = match b {
                        b'[' => {
                            self.seq.clear();
                            State::Csi
                        }
                        b']' | b'P' | b'^' | b'_' => State::StringSeq,
                        0x1B => State::Escape, // ESC ESC: stay pending
                        _ => State::Ground,    // 2-byte escape (incl. ST's `\`): dropped
                    };
                }
                State::Csi => {
                    data = &data[1..];
                    if (0x40..=0x7E).contains(&b) {
                        if b == b'm' {
                            let seq = std::mem::take(&mut self.seq);
                            self.apply_sgr(&seq);
                        }
                        self.state = State::Ground;
                    } else if self.seq.len() < 64 {
                        self.seq.push(b); // bound hostile parameter lengths
                    }
                }
                State::StringSeq => {
                    data = &data[1..];
                    match b {
                        0x07 => self.state = State::Ground,
                        0x1B => self.state = State::Escape, // ESC \ ends it in Escape
                        _ => {}
                    }
                }
            }
        }
    }

    /// Drain completed lines (called between reads by the previewer thread).
    pub fn take_lines(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.lines)
    }

    /// Close out the final partial line at end of stream.
    pub fn finish(&mut self) {
        if !self.cur.is_empty() || !self.spans.is_empty() {
            self.flush_span();
            let spans = std::mem::take(&mut self.spans);
            self.lines.push(Line { spans });
        }
    }

    fn end_line(&mut self) {
        self.flush_span();
        let spans = std::mem::take(&mut self.spans);
        self.lines.push(Line { spans });
    }

    /// Move `cur[span_start..]` into a `Span` with the current style.
    fn flush_span(&mut self) {
        if self.cur.len() > self.span_start {
            let text = self.cur.split_off(self.span_start);
            self.spans.push(Span { text, style: self.style });
            self.span_start = 0;
        }
    }

    /// Apply one SGR sequence's parameters (bytes between `ESC[` and `m`).
    fn apply_sgr(&mut self, params: &[u8]) {
        let text = std::str::from_utf8(params).unwrap_or("");
        if text.is_empty() {
            self.style = Style::default(); // bare ESC[m == reset
            return;
        }
        // Missing params default to 0 per spec (e.g. `ESC[;m`).
        let vals: Vec<u16> =
            text.split([';', ':']).map(|s| s.parse::<u16>().unwrap_or(0)).collect();
        let mut i = 0;
        while i < vals.len() {
            match vals[i] {
                0 => self.style = Style::default(),
                1 => self.style.attrs.bold = true,
                2 => self.style.attrs.dim = true,
                3 => self.style.attrs.italic = true,
                4 => self.style.attrs.underline = true,
                7 => self.style.attrs.reverse = true,
                22 => {
                    self.style.attrs.bold = false;
                    self.style.attrs.dim = false;
                }
                23 => self.style.attrs.italic = false,
                24 => self.style.attrs.underline = false,
                27 => self.style.attrs.reverse = false,
                30..=37 => self.style.fg = Color::Idx((vals[i] - 30) as u8),
                39 => self.style.fg = Color::Default,
                40..=47 => self.style.bg = Color::Idx((vals[i] - 40) as u8),
                49 => self.style.bg = Color::Default,
                90..=97 => self.style.fg = Color::Idx((vals[i] - 90 + 8) as u8),
                100..=107 => self.style.bg = Color::Idx((vals[i] - 100 + 8) as u8),
                38 | 48 => {
                    let consumed = match vals.get(i + 1) {
                        Some(2) => {
                            let rgb = (
                                *vals.get(i + 2).unwrap_or(&0) as u8,
                                *vals.get(i + 3).unwrap_or(&0) as u8,
                                *vals.get(i + 4).unwrap_or(&0) as u8,
                            );
                            let c = Color::Rgb(rgb.0, rgb.1, rgb.2);
                            if vals[i] == 38 { self.style.fg = c } else { self.style.bg = c }
                            5 // 38;2;r;g;b
                        }
                        Some(5) => {
                            // 256-color palette; the ui layer maps to SGR 38;5;n.
                            let c = Color::Idx(*vals.get(i + 2).unwrap_or(&0) as u8);
                            if vals[i] == 38 { self.style.fg = c } else { self.style.bg = c }
                            3 // 38;5;n
                        }
                        _ => vals.len(), // malformed tail: stop parsing
                    };
                    i += consumed;
                }
                _ => {} // unsupported SGR code: ignored
            }
            i += 1;
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        b if b & 0xE0 == 0xC0 => 2,
        b if b & 0xF0 == 0xE0 => 3,
        b if b & 0xF8 == 0xF0 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Vec<Line> {
        let mut f = AnsiFeeder::new();
        f.feed(input.as_bytes());
        f.finish();
        f.take_lines()
    }

    fn plain(input: &str) -> String {
        parse(input).into_iter().map(|l| l.text()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(plain("hello\nworld"), "hello\nworld");
        assert_eq!(plain(""), "");
        assert_eq!(plain("no trailing newline"), "no trailing newline");
    }

    #[test]
    fn sgr_basic_colors() {
        let lines = parse("\x1b[31mred\x1b[0m plain");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "red plain");
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].style.fg, Color::Idx(1));
        assert_eq!(lines[0].spans[1].style.fg, Color::Default);
    }

    #[test]
    fn sgr_attributes() {
        let lines = parse("\x1b[1;4mbold under\x1b[22m only under\x1b[24m off");
        let s = &lines[0];
        assert!(s.spans[0].style.attrs.bold && s.spans[0].style.attrs.underline);
        assert!(!s.spans[1].style.attrs.bold && s.spans[1].style.attrs.underline);
        assert!(!s.spans[2].style.attrs.underline);
    }

    #[test]
    fn sgr_truecolor_and_palette() {
        let lines = parse("\x1b[38;2;10;20;30mrgb\x1b[38;5;196midx");
        let s = &lines[0];
        assert_eq!(s.spans[0].style.fg, Color::Rgb(10, 20, 30));
        assert_eq!(s.spans[1].style.fg, Color::Idx(196));
    }

    #[test]
    fn other_csi_and_osc_stripped() {
        // Cursor movement: dropped entirely.
        assert_eq!(plain("a\x1b[2;3Hb"), "ab");
        // OSC 8 hyperlink: sequence dropped, link text kept.
        assert_eq!(plain("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
        // BEL-terminated OSC.
        assert_eq!(plain("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn split_across_chunks() {
        let mut f = AnsiFeeder::new();
        f.feed(b"\x1b[3");
        f.feed(b"1mred\n");
        f.finish();
        let lines = f.take_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text(), "red");
        assert_eq!(lines[0].spans[0].style.fg, Color::Idx(1));
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut f = AnsiFeeder::new();
        let s = "你好";
        let bytes = s.as_bytes();
        f.feed(&bytes[..2]); // splits the first 3-byte char
        f.feed(&bytes[2..]);
        f.finish();
        assert_eq!(f.take_lines()[0].text(), "你好");
    }

    #[test]
    fn malformed_never_panics() {
        for s in
            ["\x1b", "\x1b[", "\x1b[999999999999m", "\x1b]unterminated", "\x1b[38;2;1;", "\x1b[;;m", "text\x1b[38;5m more"]
        {
            let _ = parse(s);
        }
        // Raw invalid bytes.
        for b in [b"\xff\xfe".as_slice(), b"\x1b[31\x00m", b"\xc3", b"\xe4\xbd"] {
            let mut f = AnsiFeeder::new();
            f.feed(b);
            f.finish();
            let _ = f.take_lines();
        }
    }

    #[test]
    fn blank_lines_preserved() {
        let lines = parse("a\n\nb\n");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].spans.is_empty());
    }
}
