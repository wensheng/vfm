//! Host terminal detection: sorts the terminal vfm runs in into a [`Kind`],
//! and finds out whether it can show kitty graphics.
//!
//! Vivid advertises itself through two environment variables, so it is known
//! at once; whether it can show graphics is still probed, like any other
//! terminal. Kitty graphics support is probed rather than guessed from the
//! terminal's name, because kitty, Ghostty, Konsole and others all implement
//! the protocol. [`detect`] follows the protocol spec: it sends a
//! graphics *query action* followed by DA1 (primary device attributes).
//! Terminals answer in order and practically all of them answer DA1, so a DA1
//! reply without a graphics reply before it means "unsupported" — no timeout
//! involved. A terminal that doesn't speak the protocol discards the query
//! silently, as it does any unknown APC string.
//!
//! The replies arrive on the tty input queue, which crossterm also reads and
//! would misparse: it turns an unknown `ESC x` into Alt+x, so a graphics reply
//! would become a burst of keystrokes. The probe thread therefore owns tty
//! input until it reports: `App` doesn't poll crossterm meanwhile, and keys
//! typed in that window come back as [`Report::typeahead`] instead of being
//! lost. The thread reads its own nonblocking descriptor, so it always quits
//! by [`DEADLINE`] instead of lingering in `read()` to steal later input.
//!
//! Unix probes read the tty directly. Windows probes temporarily enable VT
//! input and collect console key records before crossterm handles normal input.

// Rust guideline compliant 2026-10-07

#[cfg(any(not(windows), test))]
use crossterm::event::KeyModifiers;
use crossterm::event::{KeyCode, KeyEvent};
use std::ffi::OsStr;
#[cfg(not(windows))]
use std::fs::File;
#[cfg(any(not(windows), test))]
use std::io::Read;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Kitty graphics *query action* for a 1x1 RGB pixel sent inline, then DA1.
///
/// This is the probe the protocol spec recommends: `a=q` makes the terminal
/// validate the image and reply without storing it. The id must match
/// [`QUERY_ID`].
const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c";

/// The `i=` key of [`QUERY`], echoed back in the terminal's graphics reply.
const QUERY_ID: &[u8] = b"i=31";

/// How long the probe waits for the DA1 reply before giving up.
///
/// Terminals answer within one round trip, so this only matters on links
/// slower than this or for terminals that never answer. Keys typed meanwhile
/// are delayed, not lost. Shorter risks a slow reply arriving once crossterm
/// owns the tty, where it would be misread as keystrokes; longer stalls input
/// on terminals that stay silent.
const DEADLINE: Duration = Duration::from_secs(1);

/// Pause between reads of the nonblocking tty while no input is pending.
///
/// Local terminals reply within a millisecond, so 2 ms adds little latency
/// while bounding the loop to about 500 wakeups before [`DEADLINE`].
#[cfg(any(not(windows), test))]
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Terminal category vfm is running inside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The Vivid terminal, whether or not it shows graphics.
    Vivid,
    /// Any other terminal that answers the kitty graphics protocol query.
    KittyGraphics,
    /// Anything else, including terminals the probe couldn't reach.
    #[default]
    Other,
}

impl Kind {
    /// The category once the probe has found whether graphics work.
    ///
    /// Vivid stays Vivid either way; any other terminal is sorted by the result.
    pub fn settle(self, graphics: bool) -> Kind {
        match self {
            Kind::Vivid => Kind::Vivid,
            _ if graphics => Kind::KittyGraphics,
            _ => Kind::Other,
        }
    }
}

/// What the probe found once it finished reading the tty.
#[derive(Debug, Default)]
pub struct Report {
    /// The terminal answered the kitty graphics query.
    pub graphics: bool,
    /// Keys typed while the probe owned tty input, to be handled in order.
    pub typeahead: Vec<KeyEvent>,
}

/// Handle to the running probe; its [`Report`] arrives over a channel.
#[derive(Debug)]
pub struct Probe {
    rx: Receiver<Report>,
}

impl Probe {
    /// Waits up to `timeout` for the report; `None` while still probing.
    pub fn wait(&self, timeout: Duration) -> Option<Report> {
        match self.rx.recv_timeout(timeout) {
            Ok(report) => Some(report),
            Err(RecvTimeoutError::Timeout) => None,
            // The thread died without reporting; nothing else will come.
            Err(RecvTimeoutError::Disconnected) => Some(Report::default()),
        }
    }
}

/// Classifies the host terminal and starts probing it for graphics.
///
/// Returns [`Kind::Vivid`] or [`Kind::Other`] from the environment, plus a
/// [`Probe`] unless no tty can be probed. Pass its report to [`Kind::settle`]
/// for the final category. Nothing else may read tty input until it arrives.
///
/// Call this with raw mode on, so the replies are neither echoed nor held for
/// a newline, and before the first frame, which paints over anything a
/// confused terminal prints for the query. Don't exit before the report
/// either, or late replies land in the shell as typed input.
///
/// # Panics
///
/// If the probe thread can't be spawned.
pub fn detect() -> (Kind, Option<Probe>) {
    let control = std::env::var_os("VIVID_ENDPOINT_CONTROL");
    let secret = std::env::var_os("VIVID_ROOT_SECRET");
    let kind = if vivid_advertised(control.as_deref(), secret.as_deref()) { Kind::Vivid } else { Kind::Other };
    (kind, spawn_probe())
}

/// Whether Vivid's endpoint is advertised: both variables set and non-empty.
///
/// Only presence matters; the secret's value is never kept.
fn vivid_advertised(control: Option<&OsStr>, secret: Option<&OsStr>) -> bool {
    [control, secret].into_iter().all(|v| v.is_some_and(|v| !v.is_empty()))
}

/// Writes [`QUERY`] and starts the thread that collects the replies.
#[cfg(not(windows))]
fn spawn_probe() -> Option<Probe> {
    let (mut reader, mut writer) = open_tty()?;
    writer.write_all(QUERY).ok()?;
    let deadline = Instant::now() + DEADLINE;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("vfm-term".into())
        .spawn(move || {
            // A failed send only means the app already quit.
            let _ = tx.send(collect(&mut reader, deadline));
        })
        .expect("spawn terminal probe");
    Some(Probe { rx })
}

/// Opens the controlling terminal as a nonblocking reader and a writer.
///
/// Each `open()` makes its own file description, so `O_NONBLOCK` stays off
/// crossterm's descriptor, and the writer blocks the way frames do, so the
/// query can't go out torn.
#[cfg(unix)]
fn open_tty() -> Option<(File, File)> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    const TTY: &str = "/dev/tty";
    let reader = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(TTY).ok()?;
    let writer = OpenOptions::new().write(true).open(TTY).ok()?;
    Some((reader, writer))
}

/// No tty to probe outside Unix.
#[cfg(not(any(unix, windows)))]
fn open_tty() -> Option<(File, File)> {
    None
}

/// Restores native keyboard input after Windows has delivered the probe replies.
#[cfg(windows)]
struct ConsoleMode {
    mode: crossterm_winapi::ConsoleMode,
    original: u32,
}

#[cfg(windows)]
impl Drop for ConsoleMode {
    fn drop(&mut self) {
        let _ = self.mode.set_mode(self.original);
    }
}

#[cfg(windows)]
fn spawn_probe() -> Option<Probe> {
    use crossterm_winapi::Handle;
    let mode = crossterm_winapi::ConsoleMode::from(Handle::current_in_handle().ok()?);
    let original = mode.mode().ok()?;
    // ENABLE_VIRTUAL_TERMINAL_INPUT preserves APC/CSI replies through ConPTY.
    mode.set_mode(original | 0x0200).ok()?;
    let guard = ConsoleMode { mode, original };
    let mut out = io::stdout().lock();
    out.write_all(QUERY).ok()?;
    out.flush().ok()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("vfm-term".into())
        .spawn(move || {
            let mut scan = ConsoleScan::default();
            let deadline = Instant::now() + DEADLINE;
            while !scan.da1 && Instant::now() < deadline {
                match crossterm::event::poll(deadline.saturating_duration_since(Instant::now())) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(crossterm::event::Event::Key(key)) => scan.feed(key),
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    _ => break,
                }
            }
            scan.keys.append(&mut scan.pending);
            drop(guard); // Restore native input before the app resumes polling it.
            let _ = tx.send(Report { graphics: scan.graphics, typeahead: scan.keys });
        })
        .ok()?;
    Some(Probe { rx })
}

/// Windows can omit the ESC records around APC/CSI replies; retain original
/// key events until a complete reply is recognized so typeahead keeps modifiers.
#[cfg(windows)]
#[derive(Default)]
struct ConsoleScan {
    pending: Vec<KeyEvent>,
    keys: Vec<KeyEvent>,
    graphics: bool,
    da1: bool,
}

#[cfg(windows)]
impl ConsoleScan {
    fn feed(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        self.pending.push(key);
        let text: String = self.pending.iter().filter_map(|key| key.code.as_char()).collect();
        // Replies are short control strings; this also bounds ambiguous typeahead.
        if self.pending.len() > 512 {
            self.keys.append(&mut self.pending);
            return;
        }
        if text.starts_with("_G") && text.ends_with('\\') {
            self.graphics |= answers_query(&text.as_bytes()[2..text.len() - 1]);
            self.pending.clear();
            return;
        }
        if text.starts_with("[?")
            && text.ends_with('c')
            && text[2..text.len() - 1].bytes().all(|b| b.is_ascii_digit() || b == b';')
        {
            self.da1 = true;
            self.pending.clear();
            return;
        }
        let candidate = text.is_empty() && key.code == KeyCode::Esc
            || text == "_"
            || text.starts_with("_G")
            || text == "["
            || text.starts_with("[?");
        if !candidate {
            self.keys.append(&mut self.pending);
        }
    }
}

/// Reads tty input until the DA1 reply, EOF, a read error or `deadline`.
#[cfg(any(not(windows), test))]
fn collect(tty: &mut impl Read, deadline: Instant) -> Report {
    let mut scan = Scan::default();
    let mut pending = Vec::new();
    // Replies are ~20 bytes each; more input just takes another read.
    let mut chunk = [0; 256];
    while !scan.da1 && Instant::now() < deadline {
        match tty.read(&mut chunk) {
            Ok(0) => break, // the terminal hung up
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                scan.feed(&mut pending);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL_INTERVAL),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // Nothing more is expected, so a lone trailing ESC was the Esc key.
    if pending == [0x1b] {
        scan.keys.push(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }
    Report { graphics: scan.graphics, typeahead: scan.keys }
}

/// What the probe has learned from tty input so far.
#[derive(Debug, Default)]
#[cfg(any(not(windows), test))]
struct Scan {
    graphics: bool,
    da1: bool,
    keys: Vec<KeyEvent>,
}

#[cfg(any(not(windows), test))]
impl Scan {
    /// Consumes every complete token in `buf`, leaving an incomplete tail.
    fn feed(&mut self, buf: &mut Vec<u8>) {
        let mut at = 0;
        while let Some((token, len)) = next_token(&buf[at..]) {
            at += len;
            match token {
                Token::Graphics => self.graphics = true,
                Token::Da1 => self.da1 = true,
                Token::Key(k) => self.keys.push(k),
                Token::Skip => {}
            }
        }
        buf.drain(..at);
    }
}

/// One unit of tty input, as the probe sees it.
#[derive(Debug, PartialEq, Eq)]
#[cfg(any(not(windows), test))]
enum Token {
    /// A kitty graphics reply to [`QUERY`].
    Graphics,
    /// The DA1 reply, which ends the probe.
    Da1,
    /// A key typed while the probe owned the tty.
    Key(KeyEvent),
    /// A complete sequence not worth replaying (mouse report, F-key, ...).
    Skip,
}

/// Splits the first token off `buf`, returning it with its length in bytes.
///
/// `None` means `buf` is empty or ends inside a sequence that more input may
/// complete. Keys decode as crossterm decodes them in raw mode, so replayed
/// typeahead hits the same bindings; rare keys (F-keys, modified arrows) are
/// skipped rather than replayed.
#[cfg(any(not(windows), test))]
fn next_token(buf: &[u8]) -> Option<(Token, usize)> {
    let plain = |code| Token::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let ctrl = |c: u8| Token::Key(KeyEvent::new(KeyCode::Char(char::from(c)), KeyModifiers::CONTROL));
    let token = match *buf.first()? {
        0x1b => return escape(buf),
        b'\r' => plain(KeyCode::Enter),
        b'\t' => plain(KeyCode::Tab),
        0x7f => plain(KeyCode::Backspace),
        0x00 => ctrl(b' '),
        c @ 0x01..=0x1a => ctrl(c - 0x01 + b'a'),
        c @ 0x1c..=0x1f => ctrl(c - 0x1c + b'4'),
        _ => return utf8(buf),
    };
    Some((token, 1))
}

/// Sequences starting with ESC: replies, special keys, or Alt+key.
#[cfg(any(not(windows), test))]
fn escape(buf: &[u8]) -> Option<(Token, usize)> {
    let plain = |code| Token::Key(KeyEvent::new(code, KeyModifiers::NONE));
    match *buf.get(1)? {
        b'[' => csi(buf),
        b'_' => apc(buf),
        b'O' => {
            let code = match *buf.get(2)? {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                _ => return Some((Token::Skip, 3)),
            };
            Some((plain(code), 3))
        }
        0x1b => Some((plain(KeyCode::Esc), 2)),
        _ => {
            let (token, len) = next_token(&buf[1..])?;
            let token = match token {
                Token::Key(mut k) => {
                    k.modifiers |= KeyModifiers::ALT;
                    Token::Key(k)
                }
                other => other,
            };
            Some((token, len + 1))
        }
    }
}

/// A CSI sequence: the DA1 reply, a special key, or something to skip.
#[cfg(any(not(windows), test))]
fn csi(buf: &[u8]) -> Option<(Token, usize)> {
    // Parameter (0x30–0x3f) and intermediate (0x20–0x2f) bytes, then a final.
    let end = 2 + buf[2..].iter().position(|b| !(0x20..=0x3f).contains(b))?;
    let (params, fin) = (&buf[2..end], buf[end]);
    if !(0x40..=0x7e).contains(&fin) {
        // Cut short by a byte that can't end a CSI: drop what came before.
        return Some((Token::Skip, end));
    }
    let plain = |code| Token::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let token = match (params, fin) {
        ([b'?', ..], b'c') => Token::Da1,
        // X10 mouse report: three raw bytes follow the final byte.
        ([], b'M') => return buf.get(..end + 4).map(|_| (Token::Skip, end + 4)),
        ([], b'A') => plain(KeyCode::Up),
        ([], b'B') => plain(KeyCode::Down),
        ([], b'C') => plain(KeyCode::Right),
        ([], b'D') => plain(KeyCode::Left),
        ([], b'H') | (b"1" | b"7", b'~') => plain(KeyCode::Home),
        ([], b'F') | (b"4" | b"8", b'~') => plain(KeyCode::End),
        ([], b'Z') => Token::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        (b"2", b'~') => plain(KeyCode::Insert),
        (b"3", b'~') => plain(KeyCode::Delete),
        (b"5", b'~') => plain(KeyCode::PageUp),
        (b"6", b'~') => plain(KeyCode::PageDown),
        _ => Token::Skip,
    };
    Some((token, end + 1))
}

/// An APC string (`ESC _ ... ESC \`), the framing of graphics replies.
#[cfg(any(not(windows), test))]
fn apc(buf: &[u8]) -> Option<(Token, usize)> {
    let body = &buf[2..];
    let end = body.iter().position(|&b| b == 0x1b || b == 0x07)?;
    let token = match body.split_first() {
        Some((b'G', reply)) if answers_query(&reply[..end - 1]) => Token::Graphics,
        _ => Token::Skip,
    };
    match (body[end], body.get(end + 1)) {
        (0x07, _) => Some((token, 2 + end + 1)),
        (_, Some(b'\\')) => Some((token, 2 + end + 2)),
        // A lone ESC at the end may still become the terminator.
        (_, None) => None,
        // Another sequence cut the string short, as in a VT parser.
        (_, Some(_)) => Some((Token::Skip, 2 + end)),
    }
}

/// Whether graphics reply data (`<keys>;<message>`) carries [`QUERY_ID`].
///
/// Any reply counts, error or `OK`: per the spec, answering at all means the
/// terminal implements the protocol.
fn answers_query(reply: &[u8]) -> bool {
    let keys = reply.split(|&b| b == b';').next().unwrap_or_default();
    keys.split(|&b| b == b',').any(|kv| kv == QUERY_ID)
}

/// A UTF-8 encoded character, `SHIFT`ed when uppercase as crossterm does.
#[cfg(any(not(windows), test))]
fn utf8(buf: &[u8]) -> Option<(Token, usize)> {
    let len = match buf[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Some((Token::Skip, 1)), // stray continuation or invalid byte
    };
    let Ok(s) = std::str::from_utf8(buf.get(..len)?) else {
        return Some((Token::Skip, 1));
    };
    let c = s.chars().next()?;
    let mods = if c.is_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
    Some((Token::Key(KeyEvent::new(KeyCode::Char(c), mods)), len))
}

#[cfg(test)]
impl Probe {
    /// A probe fed through the returned sender instead of a tty thread.
    pub fn channel() -> (mpsc::Sender<Report>, Probe) {
        let (tx, rx) = mpsc::channel();
        (tx, Probe { rx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn console_replies_accept_missing_esc_records_and_preserve_typeahead() {
        for escaped in [false, true] {
            let mut scan = ConsoleScan::default();
            let before = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
            let after = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
            scan.feed(before);
            let text = if escaped { "\x1b_Gi=31;OK\x1b\\\x1b[?62;22c" } else { "_Gi=31;OK\\[?62;22c" };
            for ch in text.chars() {
                let code = if ch == '\x1b' { KeyCode::Esc } else { KeyCode::Char(ch) };
                let key = KeyEvent::new(code, KeyModifiers::NONE);
                scan.feed(key);
                scan.feed(KeyEvent::new_with_kind(code, KeyModifiers::NONE, crossterm::event::KeyEventKind::Release));
            }
            scan.feed(after);
            assert!(scan.graphics && scan.da1);
            assert!(scan.pending.is_empty());
            assert_eq!(scan.keys, [before, after]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn console_probe_rejects_wrong_ids_and_bounds_partial_replies() {
        let mut scan = ConsoleScan::default();
        for ch in "_Gi=7;OK\\".chars() {
            scan.feed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(!scan.graphics);
        assert!(scan.pending.is_empty() && scan.keys.is_empty(), "unrelated replies are not keystrokes");
        for _ in 0..600 {
            scan.feed(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        }
        assert!(scan.pending.len() <= 512);
        let mut scan = ConsoleScan::default();
        for ch in "[?62;22c".chars() {
            scan.feed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(scan.da1 && !scan.graphics);
    }

    /// Reads `data` one byte per call, then reports no input pending.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some((&b, rest)) = self.0.split_first() else {
                return Err(io::ErrorKind::WouldBlock.into());
            };
            out[0] = b;
            self.0 = rest;
            Ok(1)
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn probe(input: &[u8]) -> Report {
        collect(&mut Trickle(input), far())
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn chr(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// kitty's replies, byte for byte (graphics.c, window.py `da1`).
    const KITTY: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[?62;52;c";

    #[test]
    fn vivid_needs_both_vars_non_empty() {
        let s = |v: &'static str| Some(OsStr::new(v));
        assert!(vivid_advertised(s("unix:/run/vivid"), s("hunter2")));
        assert!(!vivid_advertised(s("unix:/run/vivid"), None));
        assert!(!vivid_advertised(None, s("hunter2")));
        assert!(!vivid_advertised(s(""), s("hunter2")));
        assert!(!vivid_advertised(s("unix:/run/vivid"), s("")));
    }

    #[test]
    fn graphics_reply_before_da1_means_supported() {
        let r = probe(KITTY);
        assert!(r.graphics);
        assert!(r.typeahead.is_empty(), "{:?}", r.typeahead);
        // Whole-buffer reads take the same path as a byte at a time.
        assert!(collect(&mut &KITTY[..], far()).graphics);
    }

    #[test]
    fn error_reply_still_means_supported() {
        let r = probe(b"\x1b_Gi=31;EINVAL:bad format\x1b\\\x1b[?62;c");
        assert!(r.graphics);
    }

    #[test]
    fn da1_alone_means_unsupported() {
        assert!(!probe(b"\x1b[?62;22c").graphics);
        // Another image id isn't an answer to our query.
        assert!(!probe(b"\x1b_Gi=7;OK\x1b\\\x1b[?6c").graphics);
    }

    #[test]
    fn vivid_keeps_its_kind_whatever_the_probe_finds() {
        assert_eq!(Kind::Vivid.settle(true), Kind::Vivid);
        assert_eq!(Kind::Vivid.settle(false), Kind::Vivid);
        assert_eq!(Kind::Other.settle(true), Kind::KittyGraphics);
        assert_eq!(Kind::Other.settle(false), Kind::Other);
    }

    #[test]
    fn typeahead_around_replies_is_replayed_in_order() {
        let mut input = b"j\x1b[AG".to_vec();
        input.extend_from_slice(KITTY);
        input.extend_from_slice(b"q\x1b");
        // The Trickle stops at the DA1 reply, so feed everything at once:
        // keys after DA1 in the same read belong to the user too.
        let r = collect(&mut &input[..], far());
        assert!(r.graphics);
        let up = key(KeyCode::Up, KeyModifiers::NONE);
        let shift_g = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(r.typeahead, [chr('j'), up, shift_g, chr('q'), esc]);
    }

    #[test]
    fn keys_decode_like_crossterm() {
        use KeyModifiers as M;
        let cases: &[(&[u8], KeyEvent)] = &[
            (b"\r", key(KeyCode::Enter, M::NONE)),
            (b"\t", key(KeyCode::Tab, M::NONE)),
            (b"\x7f", key(KeyCode::Backspace, M::NONE)),
            (b"\x01", key(KeyCode::Char('a'), M::CONTROL)),
            (b"\n", key(KeyCode::Char('j'), M::CONTROL)),
            (b"\x00", key(KeyCode::Char(' '), M::CONTROL)),
            ("é".as_bytes(), chr('é')),
            (b"\x1bj", key(KeyCode::Char('j'), M::ALT)),
            (b"\x1b\x1b", key(KeyCode::Esc, M::NONE)),
            (b"\x1bOB", key(KeyCode::Down, M::NONE)),
            (b"\x1b[5~", key(KeyCode::PageUp, M::NONE)),
            (b"\x1b[Z", key(KeyCode::BackTab, M::SHIFT)),
        ];
        for (bytes, want) in cases {
            let r = probe(&[*bytes, &b"\x1b[?62c"[..]].concat());
            assert_eq!(r.typeahead, [*want], "{bytes:?}");
        }
    }

    #[test]
    fn mouse_reports_are_skipped() {
        let r = probe(b"\x1b[<35;10;5M\x1b[M !!\x1b[?62c");
        assert!(r.typeahead.is_empty(), "{:?}", r.typeahead);
    }

    #[test]
    fn cut_short_apc_does_not_swallow_da1() {
        let r = probe(b"\x1b_Gi=31;O\x1b[?62c");
        assert!(!r.graphics);
        assert!(r.typeahead.is_empty(), "{:?}", r.typeahead);
    }

    #[test]
    fn silent_terminal_gives_up_at_deadline() {
        let t0 = Instant::now();
        let r = collect(&mut Trickle(b"k"), t0 + Duration::from_millis(30));
        assert!(t0.elapsed() >= Duration::from_millis(30));
        assert!(!r.graphics);
        assert_eq!(r.typeahead, [chr('k')]);
    }

    #[test]
    fn hangup_ends_probe() {
        let r = collect(&mut &b"k"[..], far());
        assert!(!r.graphics);
        assert_eq!(r.typeahead, [chr('k')]);
    }

    #[test]
    fn dead_probe_thread_reports_other() {
        let (tx, p) = Probe::channel();
        assert!(p.wait(Duration::ZERO).is_none(), "still probing");
        drop(tx);
        assert_eq!(p.wait(Duration::ZERO).map(|r| r.graphics), Some(false));
    }
}
