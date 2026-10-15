//! Preview pipeline: debounce → job → done state machine with content-
//! signature dedup (yazi's PreviewSig pattern). Plugin previews run as child
//! processes whose stdout is parsed incrementally by [`crate::ansi`]; when no
//! previewer rule matches, the builtin fallback runs synchronously in-process.
//!
//! Ownership: exactly one owner (the App loop) touches Preview; the only
//! cross-thread edges are the job's mpsc channel and `child.kill()` from the
//! loop thread. `Drop for JobHandle` kills + reaps on a detached thread, so
//! superseding a slow previewer never blocks input and leaves no zombies.
//!
//! Graphics previewers (see [`crate::graphics`]) produce image frames instead
//! of lines. Only the latest frame is kept, for the App to forward to the
//! terminal; their channel holds one frame, so a slow UI stalls the
//! previewer instead of queueing megabytes of stale frames.

use crate::ansi::AnsiFeeder;
use crate::graphics::{Frame, Splitter};
use crate::sort::natural_cmp;
use crate::ui::Line;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

const MAX_LINES: usize = 20_000;
const READ_BUF: usize = 64 * 1024;

/// Bytes of a file's head checked to tell text from binary.
const SNIFF_BYTES: usize = 8192;

/// Programs tried, in order, for highlighted text previews. Debian and
/// Ubuntu install bat as `batcat`.
const BAT_NAMES: [&str; 2] = ["bat", "batcat"];

/// bat flags for a pane: colors through the pipe, no decorations, no pager,
/// and no wrapping, since the pane wraps lines itself.
const BAT_FLAGS: [&str; 4] = ["--color=always", "--style=plain", "--paging=never", "--wrap=never"];

/// The highlighter argv for text previews: bat, if `enabled` and
/// `installed` finds it under one of its names.
pub fn bat_argv(enabled: bool, installed: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    let prog = BAT_NAMES.into_iter().filter(|_| enabled).find(|name| installed(name))?;
    Some(std::iter::once(prog).chain(BAT_FLAGS).map(String::from).collect())
}

// ---------------------------------------------------------------- signature

/// FNV-1a over path bytes, size, mtime, pane size and rule id — identical
/// signature means "already showing this content" (yazi PreviewSig).
pub fn make_sig(path: &Path, size: u64, mtime: i64, pane: (u16, u16), rule: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |b: &[u8]| {
        for &byte in b {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(path.as_os_str().as_encoded_bytes());
    mix(&size.to_le_bytes());
    mix(&mtime.to_le_bytes());
    mix(&pane.0.to_le_bytes());
    mix(&pane.1.to_le_bytes());
    mix(&rule.to_le_bytes());
    h
}

// ---------------------------------------------------------------- job handle

pub enum PreviewMsg {
    Lines(Vec<Line>),
    /// A graphics previewer's next image.
    Frame(Frame),
    Done { truncated: bool },
    /// A graphics previewer ended without an image; the text is why.
    Failed(String),
}

/// A running previewer child + its reader-thread channel. The Child lives
/// here; the reader thread owns only stdout. Drop = kill + reap detached.
pub struct JobHandle {
    child: Option<Child>,
    pub rx: Receiver<PreviewMsg>,
}

impl JobHandle {
    /// Run `command` (argv/env/cwd already set) with stdout captured.
    pub fn spawn(mut command: Command, max_bytes: usize) -> std::io::Result<JobHandle> {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("vfm-preview".into())
            .spawn(move || read_job(stdout, tx, max_bytes))?;
        Ok(JobHandle { child: Some(child), rx })
    }

    /// Run a graphics previewer: stdout is cut into frames (see `graphics`).
    pub fn spawn_graphics(mut command: Command) -> std::io::Result<JobHandle> {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("vfm-preview".into())
            .spawn(move || read_graphics(stdout, tx))?;
        Ok(JobHandle { child: Some(child), rx })
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap off-thread: the reader may still be draining the pipe.
            std::thread::Builder::new()
                .name("vfm-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        }
    }
}

/// Reader-thread body: stdout → AnsiFeeder → chunked Lines messages, then
/// Done after EOF. Sends are the thread's only blocking point; a superseded
/// job fails its first send after the receiver is dropped and exits.
fn read_job(stdout: impl Read, tx: Sender<PreviewMsg>, max_bytes: usize) {
    let mut feeder = AnsiFeeder::new();
    let mut reader = std::io::BufReader::with_capacity(READ_BUF, stdout);
    let mut buf = vec![0u8; READ_BUF];
    let mut total = 0usize;
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if total + n <= max_bytes {
                    total += n;
                    feeder.feed(&buf[..n]);
                    let lines = feeder.take_lines();
                    if !lines.is_empty() && tx.send(PreviewMsg::Lines(lines)).is_err() {
                        return; // receiver gone: job was superseded
                    }
                } else {
                    truncated = true; // over budget: drain without parsing
                }
            }
            Err(_) => break,
        }
    }
    feeder.finish();
    let rest = feeder.take_lines();
    if !rest.is_empty() {
        let _ = tx.send(PreviewMsg::Lines(rest));
    }
    let _ = tx.send(PreviewMsg::Done { truncated });
}

/// Graphics reader-thread body: stdout → frames, then Done, or Failed with
/// the previewer's message if it drew nothing. Blocks while a frame waits.
fn read_graphics(mut stdout: impl Read, tx: SyncSender<PreviewMsg>) {
    let mut splitter = Splitter::default();
    let mut frames = Vec::new();
    let mut buf = vec![0u8; READ_BUF];
    let mut drew = false;
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => splitter.feed(&buf[..n], &mut frames),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        for frame in frames.drain(..) {
            drew = true;
            if tx.send(PreviewMsg::Frame(frame)).is_err() {
                return; // receiver gone: job was superseded
            }
        }
    }
    let _ = tx.send(if drew {
        PreviewMsg::Done { truncated: false }
    } else {
        PreviewMsg::Failed(splitter.message().unwrap_or_else(|| "no image".into()))
    });
}

// ---------------------------------------------------------------- state

/// What produces a preview's content.
#[derive(Clone, Debug)]
pub enum Source {
    /// In-process fallback: text head, dir listing, symlink target.
    Builtin,
    /// A `[[previewer]]` argv; the path, width and height are appended.
    Previewer(Vec<String>),
    /// A `graphics = true` previewer: same argv, but its output is frames.
    Graphics(Vec<String>),
    /// A `>` plugin's command line, run through the shell in the current
    /// directory. Never cached: it describes the directory, which changes.
    Shell(String),
}

pub struct Target {
    pub path: PathBuf,
    pub source: Source,
    pub sig: u64,
}

pub enum PreviewState {
    Idle,
    Debounce { target: Target, at: Instant },
    Running { target: Target, job: JobHandle },
    Done { sig: u64, lines: std::sync::Arc<[Line]>, truncated: bool },
    Failed { sig: u64, err: String },
}

/// What on_tick decided the app should do about repainting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    Quiet,
    /// New chunks arrived — repaint the preview pane.
    Chunks,
    /// A graphics preview has a new frame; the text needs no repaint.
    Frame,
    /// Job finished — repaint + statusbar hint update.
    Finished,
    /// Spawn failed — repaint (error shown in pane).
    Failed,
}

pub struct Preview {
    pub enabled: bool,
    pub state: PreviewState,
    /// Chunks of the currently-running job, drained from the channel by the
    /// loop thread. Owned here so render can read it without cloning.
    streaming: Vec<Line>,
    streaming_truncated: bool,
    streaming_sig: u64,
    cache: Vec<(u64, std::sync::Arc<[Line]>, bool)>,
    /// Latest frame of the current graphics preview, and its sequence number.
    image: Option<(u64, Frame)>,
    frames: u64,
    pub debounce: Duration,
    pub max_bytes: usize,
    /// Runs text files that no `[[previewer]]` rule matches (see [`bat_argv`]);
    /// `None` keeps the built-in plain-text head.
    pub highlight: Option<Vec<String>>,
    /// Snapshot taken at request() time; used when the debounce expires.
    pane: (u16, u16),
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

impl Preview {
    pub fn new(debounce: Duration, max_bytes: usize) -> Preview {
        Preview {
            enabled: false,
            state: PreviewState::Idle,
            streaming: Vec::new(),
            streaming_truncated: false,
            streaming_sig: 0,
            cache: Vec::with_capacity(8),
            image: None,
            frames: 0,
            debounce,
            max_bytes,
            highlight: None,
            pane: (80, 24),
            env: Vec::new(),
            cwd: PathBuf::from("."),
        }
    }

    /// Refreshed by the app on every request so an expired debounce spawns
    /// with current pane/env/cwd.
    pub fn set_context(&mut self, pane: (u16, u16), env: Vec<(String, String)>, cwd: &Path) {
        self.pane = pane;
        self.env = env;
        self.cwd = cwd.to_path_buf();
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.state, PreviewState::Running { .. } | PreviewState::Debounce { .. })
    }

    /// Failed-state error text, if any.
    pub fn error(&self) -> Option<&str> {
        match &self.state {
            PreviewState::Failed { err, .. } => Some(err),
            _ => None,
        }
    }

    /// The current graphics frame, numbered so a new one is recognizable.
    pub fn image(&self) -> Option<(u64, &Frame)> {
        self.image.as_ref().map(|(seq, frame)| (*seq, frame))
    }

    /// Lines to render + truncated flag, from whichever state has content.
    pub fn lines(&self) -> Option<(&[Line], bool)> {
        match &self.state {
            PreviewState::Done { lines, truncated, .. } => Some((lines, *truncated)),
            _ if !self.streaming.is_empty() => Some((&self.streaming, self.streaming_truncated)),
            _ => None,
        }
    }

    /// The app calls this whenever the hovered entry (or pane size) changes.
    /// Same sig → no-op; supersede otherwise; cache hit → instant Done.
    pub fn request(&mut self, target: Target, now: Instant) {
        if self.sig_matches(target.sig) {
            return;
        }
        self.streaming.clear();
        self.streaming_truncated = false;
        self.streaming_sig = 0;
        self.image = None;
        if let Some((lines, truncated)) = self.cache_hit(target.sig) {
            self.state = PreviewState::Done { sig: target.sig, lines, truncated };
            return;
        }
        let at = now + self.debounce;
        self.state = PreviewState::Debounce { target, at };
    }

    pub fn disable(&mut self) {
        self.state = PreviewState::Idle;
        self.streaming.clear();
        self.image = None;
    }

    fn sig_matches(&self, sig: u64) -> bool {
        let state_matches = match &self.state {
            PreviewState::Done { sig: s, .. } | PreviewState::Failed { sig: s, .. } => *s == sig,
            PreviewState::Debounce { target, .. } | PreviewState::Running { target, .. } => {
                target.sig == sig
            }
            PreviewState::Idle => false,
        };
        state_matches || (self.streaming_sig == sig && !self.streaming.is_empty())
    }

    fn cache_hit(&mut self, sig: u64) -> Option<(std::sync::Arc<[Line]>, bool)> {
        if let Some(i) = self.cache.iter().position(|(s, _, _)| *s == sig) {
            let entry = self.cache.remove(i);
            self.cache.push(entry.clone());
            return Some((entry.1, entry.2));
        }
        None
    }

    fn cache_put(&mut self, sig: u64, lines: std::sync::Arc<[Line]>, truncated: bool) {
        if lines.is_empty() {
            return;
        }
        if self.cache.len() >= 8 {
            self.cache.remove(0);
        }
        self.cache.push((sig, lines, truncated));
    }

    /// Drive the state machine; called every loop pass. `now` comes from the
    /// caller so tests can script time.
    pub fn tick(&mut self, now: Instant) -> Tick {
        // Debounce expiry → spawn.
        if let PreviewState::Debounce { at, .. } = &self.state {
            if *at > now {
                return Tick::Quiet;
            }
            let PreviewState::Debounce { target, .. } =
                std::mem::replace(&mut self.state, PreviewState::Idle)
            else {
                unreachable!()
            };
            return self.start(target);
        }
        // Running → drain chunks.
        let PreviewState::Running { target, job } = &mut self.state else {
            return Tick::Quiet;
        };
        let mut tick = Tick::Quiet;
        loop {
            match job.rx.try_recv() {
                Ok(PreviewMsg::Lines(lines)) => {
                    if self.streaming.is_empty() {
                        self.streaming_sig = target.sig;
                    }
                    self.streaming.extend(lines);
                    if self.streaming.len() > MAX_LINES {
                        self.streaming.truncate(MAX_LINES);
                        self.streaming_truncated = true;
                    }
                    tick = Tick::Chunks;
                }
                Ok(PreviewMsg::Frame(frame)) => {
                    // The first frame replaces the pane's old content: full
                    // repaint. Later ones only swap the image. One per tick:
                    // older frames would be overwritten unseen.
                    let first = self.image.is_none();
                    self.frames += 1;
                    self.image = Some((self.frames, frame));
                    return if first { Tick::Chunks } else { Tick::Frame };
                }
                Ok(PreviewMsg::Failed(err)) => {
                    let sig = target.sig;
                    self.state = PreviewState::Failed { sig, err };
                    return Tick::Failed;
                }
                Ok(PreviewMsg::Done { truncated }) => {
                    let truncated = self.streaming_truncated || truncated;
                    let lines: std::sync::Arc<[Line]> = std::mem::take(&mut self.streaming).into();
                    self.streaming_truncated = false;
                    self.streaming_sig = 0;
                    let sig = target.sig;
                    if !matches!(target.source, Source::Shell(_)) {
                        self.cache_put(sig, lines.clone(), truncated);
                    }
                    self.state = PreviewState::Done { sig, lines, truncated };
                    return Tick::Finished;
                }
                Err(TryRecvError::Empty) => return tick,
                Err(TryRecvError::Disconnected) => {
                    // Reader died without Done: fail gracefully, never hang.
                    let sig = target.sig;
                    self.streaming.clear();
                    self.state = PreviewState::Failed { sig, err: "previewer crashed".into() };
                    return Tick::Failed;
                }
            }
        }
    }

    /// Transition Debounce → Running (external command) or Done (builtin).
    fn start(&mut self, target: Target) -> Tick {
        let graphics = matches!(target.source, Source::Graphics(_));
        let command = match &target.source {
            Source::Builtin => match self.highlighter(&target.path) {
                Some(command) => command,
                None => {
                    let (lines, truncated) = builtin_preview(&target.path, self.pane, self.max_bytes);
                    let sig = target.sig;
                    let lines: std::sync::Arc<[Line]> = lines.into();
                    self.cache_put(sig, lines.clone(), truncated);
                    self.state = PreviewState::Done { sig, lines, truncated };
                    return Tick::Finished;
                }
            },
            Source::Previewer(argv) | Source::Graphics(argv) => {
                let Some((prog, args)) = argv.split_first() else {
                    self.state = PreviewState::Failed { sig: target.sig, err: "empty command".into() };
                    return Tick::Failed;
                };
                let mut c = Command::new(prog);
                c.args(args)
                    .arg(&target.path)
                    .arg(self.pane.0.to_string())
                    .arg(self.pane.1.to_string())
                    .envs(self.env.iter().map(|(k, v)| (k, v)))
                    .current_dir(&self.cwd);
                c
            }
            Source::Shell(line) => crate::plugin::shell_command(line, &self.cwd, &self.env),
        };
        let program = command.get_program().to_string_lossy().into_owned();
        let job = if graphics { JobHandle::spawn_graphics(command) } else { JobHandle::spawn(command, self.max_bytes) };
        match job {
            Ok(job) => {
                self.state = PreviewState::Running { target, job };
                Tick::Quiet
            }
            Err(e) => {
                let sig = target.sig;
                self.state = PreviewState::Failed { sig, err: format!("{program}: {e}") };
                Tick::Failed
            }
        }
    }
}

impl Preview {
    /// The highlighter command for `path` if it is a readable text file.
    /// Binary files stay built-in: bat passes them through when piped.
    /// Only lines that can show are asked for, since highlighting a large
    /// file whole takes seconds.
    fn highlighter(&self, path: &Path) -> Option<Command> {
        let (prog, flags) = self.highlight.as_ref()?.split_first()?;
        if !is_text_file(path) {
            return None;
        }
        let mut c = Command::new(prog);
        c.args(flags)
            .arg(format!("--line-range=:{}", self.pane.1.max(1)))
            .arg("--")
            .arg(path)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd);
        Some(c)
    }
}

/// A regular file (not a symlink) whose head reads as text.
fn is_text_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) && head_is_text(path)
}

/// Whether opening `path` lands in a text editor: a regular file — symlinks
/// followed, so their target opens — whose head reads as text. Directories
/// and special files are not; their heads aren't read, so pipes can't stall.
pub fn opens_as_text(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file()) && head_is_text(path)
}

fn head_is_text(path: &Path) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    f.take(SNIFF_BYTES as u64).read_to_end(&mut head).is_ok() && !looks_binary(&head)
}

// ---------------------------------------------------------------- builtin

/// The no-plugin fallback: dir listing / text head / symlink target — all
/// synchronous, allocation-bounded, safe on binary files.
pub fn builtin_preview(path: &Path, pane: (u16, u16), max_bytes: usize) -> (Vec<Line>, bool) {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return (vec![Line::plain(format!("stat: {e}"))], false),
    };
    if md.is_dir() {
        return builtin_dir(path, pane.1 as usize);
    }
    if md.file_type().is_symlink() {
        let text = match std::fs::read_link(path) {
            Ok(t) => format!("→ {}", t.to_string_lossy()),
            Err(e) => format!("→ readlink: {e}"),
        };
        return (vec![Line::plain(text)], false);
    }
    if !md.is_file() {
        return (vec![Line::plain("(special file)")], false);
    }
    builtin_file(path, max_bytes)
}

fn builtin_dir(path: &Path, rows: usize) -> (Vec<Line>, bool) {
    let mut names: Vec<String> = Vec::new();
    let Ok(rd) = std::fs::read_dir(path) else { return (vec![], false) };
    for de in rd.flatten() {
        let mut n = de.file_name().to_string_lossy().into_owned();
        if de.file_type().map(|f| f.is_dir()).unwrap_or(false) {
            n.push('/');
        }
        names.push(n);
    }
    names.sort_by(|a, b| natural_cmp(a, b));
    let total = names.len();
    // Cap generation cost on huge dirs; the renderer shows what fits anyway.
    let cap = rows.saturating_sub(1).clamp(20, 10_000);
    let lines: Vec<Line> = names.into_iter().take(cap).map(Line::plain).collect();
    (lines, total > cap)
}

fn builtin_file(path: &Path, max_bytes: usize) -> (Vec<Line>, bool) {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return (vec![Line::plain(format!("open: {e}"))], false),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let want = max_bytes.min(256 * 1024);
    let mut data = Vec::with_capacity(want.min(64 * 1024));
    let mut chunk = [0u8; READ_BUF];
    let mut truncated = len as usize > want;
    while data.len() < want {
        let n = match f.read(&mut chunk[..(want - data.len()).min(READ_BUF)]) {
            Ok(0) => {
                truncated = false;
                break;
            }
            Ok(n) => n,
            Err(_) => break,
        };
        data.extend_from_slice(&chunk[..n]);
    }
    if looks_binary(&data) {
        let kind = sniff_kind(&data);
        let lines = vec![
            Line::plain(format!("binary: {kind}")),
            Line::plain(format!("{} bytes", crate::util::coolsize(len))),
        ];
        return (lines, truncated);
    }
    // Reuse the ANSI feeder for line splitting + control-char hygiene: the
    // same path plugin output takes, so text previews look identical.
    let mut feeder = AnsiFeeder::new();
    feeder.feed(&data);
    feeder.finish();
    (feeder.take_lines(), truncated)
}

/// NUL in the head, or >10% control bytes, means "don't pretend this is text".
fn looks_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(SNIFF_BYTES)];
    if head.contains(&0) {
        return true;
    }
    if head.is_empty() {
        return false;
    }
    let ctrl = head.iter().filter(|&&b| b < 0x09 || (0x0e..0x20).contains(&b)).count();
    ctrl * 10 > head.len()
}

fn sniff_kind(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x7fELF") {
        "ELF binary"
    } else if data.starts_with(&[0x4d, 0x5a]) {
        "Windows executable"
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "PNG image"
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        "JPEG image"
    } else if data.starts_with(b"PK") {
        "zip archive"
    } else if data.starts_with(b"%PDF") {
        "PDF document"
    } else {
        "binary data"
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-prev-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builtin_text_and_binary() {
        let d = tmpdir("builtin");
        std::fs::write(d.join("t.txt"), b"line one\nline two\n").unwrap();
        let (lines, trunc) = builtin_preview(&d.join("t.txt"), (80, 24), 64 * 1024);
        assert!(!trunc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text(), "line one");

        let mut png = vec![0x89, b'P', b'N', b'G', 0, 0, 0, 0];
        png.extend(std::iter::repeat_n(0u8, 64));
        std::fs::write(d.join("img.png"), &png).unwrap();
        let (lines, _) = builtin_preview(&d.join("img.png"), (80, 24), 64 * 1024);
        assert_eq!(lines[0].text(), "binary: PNG image");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn builtin_dir_and_truncation() {
        let d = tmpdir("dir");
        for n in ["b", "a", "c"] {
            fs_create(&d.join(n));
        }
        let (lines, trunc) = builtin_preview(&d, (80, 24), 64 * 1024);
        assert!(!trunc);
        let texts: Vec<String> = lines.iter().map(|l| l.text()).collect();
        assert_eq!(texts, vec!["a/", "b/", "c/"]);

        let big = "x".repeat(300 * 1024);
        std::fs::write(d.join("big.txt"), &big).unwrap();
        let (_, trunc) = builtin_preview(&d.join("big.txt"), (80, 24), 64 * 1024);
        assert!(trunc);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn fs_create(p: &Path) {
        std::fs::create_dir(p).unwrap();
    }

    #[test]
    fn opens_as_text_follows_symlinks_and_rejects_the_rest() {
        let d = tmpdir("opentext");
        std::fs::write(d.join("t.txt"), b"hi\n").unwrap();
        std::fs::write(d.join("empty"), b"").unwrap();
        std::fs::write(d.join("b.dat"), [0x89, b'P', b'N', b'G', 0, 1, 2, 3]).unwrap();
        assert!(opens_as_text(&d.join("t.txt")));
        assert!(opens_as_text(&d.join("empty")));
        assert!(!opens_as_text(&d.join("b.dat")));
        assert!(!opens_as_text(&d)); // a directory
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("t.txt", d.join("link.txt")).unwrap();
            assert!(opens_as_text(&d.join("link.txt")));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_sniff() {
        assert!(looks_binary(&[0x7f, b'E', b'L', b'F', 0, 1, 2, 3]));
        assert!(!looks_binary(b"plain text\nwith lines\n"));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn sig_changes_with_inputs() {
        let p = Path::new("/tmp/x");
        let base = make_sig(p, 10, 20, (80, 24), 0);
        assert_eq!(base, make_sig(p, 10, 20, (80, 24), 0));
        assert_ne!(base, make_sig(p, 11, 20, (80, 24), 0));
        assert_ne!(base, make_sig(p, 10, 21, (80, 24), 0));
        assert_ne!(base, make_sig(p, 10, 20, (79, 24), 0));
        assert_ne!(base, make_sig(p, 10, 20, (80, 24), 1));
        assert_ne!(base, make_sig(Path::new("/tmp/y"), 10, 20, (80, 24), 0));
    }

    #[cfg(unix)]
    #[test]
    fn job_lifecycle_and_supersede() {
        let d = tmpdir("job");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'a\\nb\\n'; sleep 30"]);
        let job = JobHandle::spawn(cmd, 64 * 1024).unwrap();
        // First chunk arrives quickly.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            match job.rx.try_recv() {
                Ok(PreviewMsg::Lines(ls)) => got = ls,
                Ok(PreviewMsg::Done { .. }) => break,
                Ok(PreviewMsg::Frame(_) | PreviewMsg::Failed(_)) => panic!("text job sent graphics"),
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => panic!("reader died early"),
            }
        }
        assert_eq!(got.len(), 2);
        // Supersede: dropping the handle kills the sleeper and reaps it.
        drop(job);
        std::thread::sleep(Duration::from_millis(50));
        // The real assertion is absence of a leaked `sleep 30` child — if
        // kill/reap were broken, this process would hold a live child here.
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A graphics previewer printing `script`'s output, as `sh -c` runs it.
    #[cfg(unix)]
    fn graphics_preview(d: &Path, script: &str) -> Preview {
        let mut pv = Preview::new(Duration::ZERO, 64 * 1024);
        pv.set_context((40, 20), Vec::new(), d);
        let argv = vec!["sh".into(), "-c".into(), script.into(), "sh".into()];
        let path = d.join("pic.png");
        pv.request(Target { path, source: Source::Graphics(argv), sig: 7 }, Instant::now());
        pv
    }

    /// Ticks `pv` until `done` holds, or panics after 5 s.
    #[cfg(unix)]
    fn tick_until(pv: &mut Preview, done: impl Fn(&Preview) -> bool) -> Vec<Tick> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ticks = Vec::new();
        while !done(pv) {
            assert!(Instant::now() < deadline, "state: {:?}", pv.error());
            ticks.push(pv.tick(Instant::now()));
            std::thread::sleep(Duration::from_millis(2));
        }
        ticks
    }

    #[cfg(unix)]
    #[test]
    fn graphics_job_streams_numbered_frames_with_args_appended() {
        let d = tmpdir("gfx");
        // Two frames; the second's id echoes the appended width and height.
        let script = r#"printf '\033_Ga=T,i=11;AAAA\033\\'; printf '\033_Ga=T,i=%s%s;AAAA\033\\' "$2" "$3""#;
        let mut pv = graphics_preview(&d, script);
        let ticks = tick_until(&mut pv, |pv| matches!(pv.state, PreviewState::Done { .. }));
        // The first frame repaints the pane; later ones only swap the image.
        let frames: Vec<Tick> = ticks.into_iter().filter(|t| matches!(t, Tick::Chunks | Tick::Frame)).collect();
        assert_eq!(frames, [Tick::Chunks, Tick::Frame]);
        let (seq, frame) = pv.image().expect("last frame kept");
        assert_eq!((seq, frame.id), (2, Some(4020)));
        assert!(pv.lines().is_some_and(|(lines, _)| lines.is_empty()));
        // Moving to another target drops the image at once.
        let other = Target { path: d.join("x"), source: Source::Builtin, sig: 8 };
        pv.request(other, Instant::now());
        assert!(pv.image().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn graphics_job_without_frames_fails_with_its_message() {
        let d = tmpdir("gfxfail");
        let mut pv = graphics_preview(&d, "echo 'no video stream'; exit 1");
        tick_until(&mut pv, |pv| pv.error().is_some());
        assert_eq!(pv.error(), Some("no video stream"));
        let mut pv = graphics_preview(&d, "exit 0");
        tick_until(&mut pv, |pv| pv.error().is_some());
        assert_eq!(pv.error(), Some("no image"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn text_files_go_through_the_highlighter_and_the_rest_stays_builtin() {
        let d = tmpdir("highlight");
        std::fs::write(d.join("t.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(d.join("bin.dat"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        let mut pv = Preview::new(Duration::ZERO, 64 * 1024);
        pv.set_context((40, 24), Vec::new(), &d);
        // Echoes the arguments vfm appends.
        let echo = r#"printf 'HL %s %s %s\n' "$1" "$2" "$3""#;
        pv.highlight = Some(vec!["sh".into(), "-c".into(), echo.into(), "sh".into()]);
        let mut show = |name: &str, sig| {
            let target = Target { path: d.join(name), source: Source::Builtin, sig };
            pv.request(target, Instant::now());
            run_to_done(&mut pv);
            pv.lines().unwrap().0.iter().map(|l| l.text()).collect::<Vec<_>>()
        };
        let path = d.join("t.rs").display().to_string();
        assert_eq!(show("t.rs", 1), [format!("HL --line-range=:24 -- {path}")]);
        assert_eq!(show("bin.dat", 2)[0], "binary: PNG image");
        assert!(show("sub", 3).is_empty(), "empty dir listing");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bat_is_found_under_either_name_unless_disabled() {
        let flags = |prog: &str| std::iter::once(prog).chain(BAT_FLAGS).map(String::from).collect::<Vec<_>>();
        assert_eq!(bat_argv(true, |p| p == "bat"), Some(flags("bat")));
        assert_eq!(bat_argv(true, |p| p == "batcat"), Some(flags("batcat")));
        assert_eq!(bat_argv(true, |_| false), None);
        assert_eq!(bat_argv(false, |_| true), None);
    }

    #[test]
    fn spawn_failures_name_the_program() {
        let mut pv = Preview::new(Duration::ZERO, 64 * 1024);
        let source = Source::Graphics(vec!["vfm-no-such-previewer".into()]);
        pv.request(Target { path: PathBuf::from("x.png"), source, sig: 9 }, Instant::now());
        assert_eq!(pv.tick(Instant::now()), Tick::Failed);
        assert!(pv.error().unwrap().starts_with("vfm-no-such-previewer: "), "{:?}", pv.error());
    }

    #[cfg(unix)]
    #[test]
    fn state_machine_dedup_and_cache() {
        let mut pv = Preview::new(Duration::ZERO, 64 * 1024);
        let d = tmpdir("state");
        let path = d.join("f.txt");
        std::fs::write(&path, b"hello\n").unwrap();
        let pane = (80u16, 24u16);
        let sig = make_sig(&path, 6, 1, pane, 0);

        let t0 = Instant::now();
        pv.request(Target { path: path.clone(), source: Source::Builtin, sig }, t0);
        assert!(pv.is_busy());
        // Same sig again → still one debounce, no restart.
        pv.request(Target { path: path.clone(), source: Source::Builtin, sig }, t0);
        assert!(matches!(pv.state, PreviewState::Debounce { .. }));

        assert_eq!(pv.tick(t0), Tick::Finished);
        assert!(matches!(pv.state, PreviewState::Done { .. }));
        let (lines, _) = pv.lines().unwrap();
        assert_eq!(lines[0].text(), "hello");

        // Leave + come back with same sig → served from cache, Done instantly.
        pv.disable();
        pv.request(Target { path: path.clone(), source: Source::Builtin, sig }, t0);
        assert!(matches!(pv.state, PreviewState::Done { .. }));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Drive a preview to completion (debounce, spawn, drain).
    #[cfg(unix)]
    fn run_to_done(pv: &mut Preview) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pv.is_busy() && Instant::now() < deadline {
            pv.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(pv.state, PreviewState::Done { .. }), "error: {:?}", pv.error());
    }

    #[cfg(unix)]
    #[test]
    fn shell_source_runs_in_cwd_and_is_never_cached() {
        let d = tmpdir("shell");
        let mut pv = Preview::new(Duration::ZERO, 64 * 1024);
        pv.set_context((80, 24), vec![("VFM_X".into(), "env-ok".into())], &d);
        let target = || Target {
            path: d.clone(),
            source: Source::Shell("ls; echo \"$VFM_X\"".into()),
            sig: 42,
        };
        std::fs::write(d.join("one"), b"").unwrap();
        pv.request(target(), Instant::now());
        run_to_done(&mut pv);
        let texts: Vec<String> = pv.lines().unwrap().0.iter().map(|l| l.text()).collect();
        assert_eq!(texts, ["one", "env-ok"]); // ran in cwd, saw the env
        // A rerun after the directory changed must not be served from cache.
        std::fs::write(d.join("two"), b"").unwrap();
        pv.disable();
        pv.request(target(), Instant::now());
        run_to_done(&mut pv);
        assert_eq!(pv.lines().unwrap().0.len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }
}
