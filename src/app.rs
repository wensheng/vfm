//! The orchestrator — the port of nnn's `browse()` loop. Single-owner state
//! mutated in exactly one place (`apply`); workers/timers talk back through
//! channels and atomics only. One `Dir` in memory at a time; contexts hold
//! just enough strings to reconstruct themselves.

use crate::action::{default_keymap, Action, ClickKind, InputEv};
use crate::config::Config;
use crate::context::{Context, CTX_MAX};
use crate::dir::Dir;
use crate::entry::{Entry, EntryFlags};
use crate::filter::{FilterMode, Matcher};
use crate::graphics;
use crate::ops::{self, OpHandle, OpKind, OpRequest};
use crate::plugin::{self, Reply, SpawnMode};
use crate::preview::{make_sig, Preview, Source, Target, Tick as PreviewTick};
use crate::selection::Selection;
use crate::term;
use crate::ui::{Attrs, Line, Screen, Span, Style, Theme};
use crate::util::{coolsize, fmt_time, now_secs, tilde_path, truncate_cols};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- events

/// Testability seam: crossterm impl in production, scripted source in tests.
pub trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

pub struct CrosstermSource;

impl EventSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        crossterm::event::poll(timeout)
    }
    fn read(&mut self) -> io::Result<Event> {
        crossterm::event::read()
    }
}

// ---------------------------------------------------------------- modes

#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    /// `/` — live filter; arrows keep navigating the filtered listing.
    Filter,
    Prompt(PromptKind),
    Confirm(ConfirmKind),
    /// `;` plugin chooser; value = highlighted index into `menu_items`.
    Menu(usize),
    /// Captured output / help scrollback.
    Pager { lines: Vec<Line>, title: String, scroll: usize },
}

#[derive(Clone, Debug)]
pub enum PromptKind {
    /// C-r: prefilled current name.
    Rename(PathBuf),
    /// n: new file (trailing '/' → dir).
    New,
    /// w: copy hovered/first-selected to a new name in cwd.
    CopyAs(PathBuf),
    /// o: shell command; the path is appended.
    OpenWith(PathBuf),
    /// J: relative jump ±n.
    Jump,
}

#[derive(Clone, Debug)]
pub enum ConfirmKind {
    Trash(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
}

/// Preview pane pinned to a `>` plugin's output until the user navigates.
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    /// Index into `cfg.plugins`.
    pub plugin: usize,
    /// Pane state to restore on unpin (it may have been opened just for this).
    pub was_enabled: bool,
}

/// Poll interval while a graphics preview animates: frames are picked up
/// between polls, so this caps playback near 60 fps. The 30 ms busy poll
/// would cap it at 33 fps and add judder to 24 and 30 fps video.
const ANIMATION_TICK: Duration = Duration::from_millis(15);

/// The preview image on screen: which frame, where, and its id for deleting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Shown {
    seq: u64,
    origin: (u16, u16),
    id: Option<u32>,
}

/// What Enter does with a hovered, non-executable file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FileOpen {
    /// Run this command line fullscreen, via `suspend_and_run`.
    Program(String),
    /// Open it in `$EDITOR` (vi by default).
    Edit,
    /// No handler: Enter does nothing.
    Nothing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Continue,
    Quit(i32),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Dirty {
    None,
    /// Only the old + new cursor entry lines changed (scroll unchanged).
    TwoLines,
    Full,
}

/// Shared single-line editor for filter/prompts (statusbar row widget).
#[derive(Default, Clone, Debug)]
pub struct InputLine {
    pub prompt: String,
    pub value: String,
    /// Cursor position in chars.
    pub cur: usize,
}

impl InputLine {
    pub fn new(prompt: &str) -> Self {
        Self { prompt: prompt.into(), ..Default::default() }
    }
    fn chars(&self) -> (usize, usize) {
        // (char index of cursor, total chars)
        (self.cur, self.value.chars().count())
    }
    pub fn insert(&mut self, c: char) {
        let byte = byte_of(&self.value, self.cur);
        self.value.insert(byte, c);
        self.cur += 1;
    }
    pub fn backspace(&mut self) {
        if self.cur == 0 {
            return;
        }
        let prev_char = self.cur - 1;
        let b_prev = byte_of(&self.value, prev_char);
        let b_cur = byte_of(&self.value, self.cur);
        self.value.replace_range(b_prev..b_cur, "");
        self.cur = prev_char;
    }
    pub fn delete(&mut self) {
        let (_, total) = self.chars();
        if self.cur >= total {
            return;
        }
        let b0 = byte_of(&self.value, self.cur);
        let b1 = byte_of(&self.value, self.cur + 1);
        self.value.replace_range(b0..b1, "");
    }
    pub fn left(&mut self) {
        self.cur = self.cur.saturating_sub(1);
    }
    pub fn right(&mut self) {
        let (_, total) = self.chars();
        if self.cur < total {
            self.cur += 1;
        }
    }
    pub fn home(&mut self) {
        self.cur = 0;
    }
    pub fn end(&mut self) {
        self.cur = self.value.chars().count();
    }
    pub fn word_back(&mut self) {
        let bytes = self.value.as_bytes();
        let mut b = byte_of(&self.value, self.cur);
        while b > 0 && bytes[b - 1].is_ascii_whitespace() {
            b -= 1;
        }
        while b > 0 && !bytes[b - 1].is_ascii_whitespace() {
            b -= 1;
        }
        self.cur = char_of(&self.value, b);
    }
    pub fn clear(&mut self) {
        self.value.clear();
        self.cur = 0;
    }
    pub fn render(&self, width: usize) -> Line {
        // Prompt + value with a cursor block; the window slides so the
        // cursor-end stays visible in narrow terminals.
        let full: Vec<char> = format!("{}{}", self.prompt, self.value).chars().collect();
        let cur_abs = self.prompt.chars().count() + self.cur;
        let (start, cur) = if width > 0 && full.len() + 1 > width {
            let start = cur_abs
                .saturating_sub(width - 1)
                .min(full.len().saturating_sub(width - 1));
            (start, cur_abs - start)
        } else {
            (0, cur_abs)
        };
        let cur = cur.min(full.len());
        let mut spans = Vec::with_capacity(3);
        spans.push(Span { text: full[start..cur].iter().collect(), style: Style::default() });
        spans.push(Span {
            text: full.get(cur).map(|c| c.to_string()).unwrap_or_else(|| " ".into()),
            style: Style { attrs: Attrs { reverse: true, ..Default::default() }, ..Default::default() },
        });
        if cur < full.len() {
            spans.push(Span { text: full[cur + 1..].iter().collect(), style: Style::default() });
        }
        Line::from_spans(spans)
    }
}

fn byte_of(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}
fn char_of(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
}

// ---------------------------------------------------------------- app

pub struct App {
    pub cfg: Config,
    pub dir: Dir,
    pub ctx: Vec<Context>,
    pub ctx_i: usize,
    /// Bit i set = context i is open (nnn: independent slots, not a count).
    pub ctx_active: u8,
    pub sel: Selection,
    pub screen: Screen,
    pub events: Box<dyn EventSource>,
    pub preview: Preview,
    pub mode: Mode,
    pub input: InputLine,
    /// Statusbar message with 3s expiry.
    pub msg: Option<(String, Instant)>,
    pub dirty: Dirty,
    /// Synthetic pre-queued action (nnn presel; also drives --bench/tests).
    pub pending: Option<Action>,
    pub op: Option<OpHandle>,
    pub auto_next: bool,
    pub detail: bool,
    pub menu_items: Vec<usize>,
    /// Written on QuitCd (VFM_CD_FILE contract, like nnn's NNN_CD_FILE).
    pub quit_cd: Option<PathBuf>,
    /// A `>` plugin's output currently owns the preview pane.
    pub preview_pin: Option<Pin>,
    pub start_dir: PathBuf,
    pub home: Option<PathBuf>,
    /// Host terminal category; `Other` until the probe (if any) reports.
    pub term: term::Kind,
    /// The terminal answered the kitty graphics probe: graphics previewers run.
    pub graphics: bool,
    /// Terminal probe in flight: its thread owns tty input until it reports.
    pub probe: Option<term::Probe>,
    /// Preview image currently on screen, if any.
    shown: Option<Shown>,
    keymap: std::collections::HashMap<KeyEvent, Action>,
    last_cursor: usize,
    last_scroll: usize,
    last_click: Option<(u16, u16, Instant)>,
    last_tick: Instant,
}

impl App {
    pub fn new(
        cfg: Config,
        start: PathBuf,
        screen: Screen,
        events: Box<dyn EventSource>,
    ) -> io::Result<App> {
        let start_dir = std::fs::canonicalize(&start)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", e, start.display())))?;
        let home = std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from));
        // All slots exist up front (a few strings each); `ctx_active` says
        // which are open, so every `ctx[ctx_i]` index is always valid.
        let ctx = (0..CTX_MAX).map(|_| Context::new(start_dir.clone(), &cfg)).collect();
        let dir = Dir::load(&start_dir, cfg.ui.time_type, cfg.ui.show_hidden)?;
        let mut keymap = default_keymap();
        let user_keys = apply_key_config(&mut keymap, &cfg.keys);
        for (i, p) in cfg.plugins.iter().enumerate() {
            match p.key.as_deref().and_then(crate::action::parse_key) {
                // An explicit [keys] binding wins over a plugin's key.
                Some(k) if !user_keys.contains(&k) => {
                    keymap.insert(k, Action::PluginRun(i));
                }
                _ => {}
            }
        }
        let mut preview = Preview::new(
            Duration::from_millis(cfg.preview.debounce_ms),
            cfg.preview.max_bytes,
        );
        preview.enabled = cfg.ui.preview && cfg.preview.enabled;
        preview.highlight = crate::preview::bat_argv(cfg.preview.bat, plugin::which);
        let mut app = App {
            detail: cfg.ui.detail,
            ctx_active: 1,
            dir,
            cfg,
            ctx,
            ctx_i: 0,
            sel: Selection::new(),
            screen,
            events,
            preview,
            mode: Mode::Normal,
            input: InputLine::default(),
            msg: None,
            dirty: Dirty::Full,
            pending: None,
            op: None,
            auto_next: false,
            menu_items: Vec::new(),
            quit_cd: None,
            preview_pin: None,
            start_dir,
            home,
            term: term::Kind::Other,
            graphics: false,
            probe: None,
            shown: None,
            keymap,
            last_cursor: 0,
            last_scroll: 0,
            last_click: None,
            last_tick: Instant::now(),
        };
        app.apply_ctx_sort_and_filter();
        app.sync_selection_flags();
        // A pane open at startup previews the first entry without a keypress.
        app.preview_on_hover();
        Ok(app)
    }

    // ------------------------------------------------------------ loop

    pub fn run(&mut self) -> Flow {
        let flow = self.event_loop();
        // Images outlive vfm's text unless deleted.
        self.hide_graphics();
        self.screen.flush();
        flow
    }

    fn event_loop(&mut self) -> Flow {
        loop {
            self.render();
            let timeout = self.poll_timeout();
            // Until the terminal probe reports, its thread owns tty input:
            // crossterm would misparse the probe's replies as keystrokes.
            let polled = match &self.probe {
                Some(probe) => match probe.wait(timeout) {
                    Some(report) => {
                        if let Some(flow) = self.settle_term(report) {
                            return flow;
                        }
                        continue;
                    }
                    None => Ok(false),
                },
                None => self.events.poll(timeout),
            };
            match polled {
                Ok(true) => match self.events.read() {
                    Ok(ev) => {
                        if let Some(flow) = self.handle_event(ev) {
                            return flow;
                        }
                    }
                    Err(e) => self.notify(format!("input: {e}")),
                },
                Ok(false) => {
                    if let Some(flow) = self.on_tick() {
                        return flow;
                    }
                }
                Err(e) => self.notify(format!("poll: {e}")),
            }
        }
    }

    /// Records the terminal probe's verdict and replays the keys it swallowed.
    fn settle_term(&mut self, report: term::Report) -> Option<Flow> {
        self.probe = None;
        self.graphics = report.graphics;
        self.term = self.term.settle(report.graphics);
        // Graphics previewers were skipped until now; re-pick the hovered one.
        if self.graphics && self.cfg.previewers.iter().any(|r| r.graphics) {
            self.preview_supersede();
        }
        report.typeahead.into_iter().find_map(|k| self.handle_event(Event::Key(k)))
    }

    /// Adaptive poll interval — the port of nnn's timeout matrix.
    fn poll_timeout(&self) -> Duration {
        if self.pending.is_some() {
            return Duration::ZERO;
        }
        if self.preview.is_busy() {
            // A running job that has drawn a frame is playing an animation.
            return if self.preview.image().is_some() { ANIMATION_TICK } else { Duration::from_millis(30) };
        }
        if self.op.is_some() {
            return Duration::from_millis(100);
        }
        if let Mode::Confirm(ConfirmKind::Delete(_)) = self.mode {
            return Duration::from_millis(1000);
        }
        Duration::from_millis(1000)
    }

    /// Timer work: preview pickup, op progress, 1 Hz dir poll, msg expiry.
    fn on_tick(&mut self) -> Option<Flow> {
        // 1. preview debounce → spawn / chunk drain
        let ptick = self.preview.tick(Instant::now());
        // A new frame only swaps the image, which render() does every pass.
        if !matches!(ptick, PreviewTick::Quiet | PreviewTick::Frame) {
            self.dirty = Dirty::Full;
        }
        // 2. op progress
        let op_done = self.op.as_ref().is_some_and(|o| o.is_done());
        if op_done {
            let handle = self.op.take().expect("op");
            let res = handle.finish();
            self.finish_op(res);
        }
        // 3. 1 Hz dir change poll (nnn's lazy inotify stand-in)
        if self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.last_tick = Instant::now();
            if self.dir.changed() {
                if let Err(e) = self.dir.repopulate() {
                    self.notify(format!("refresh: {e}"));
                } else {
                    self.sync_selection_flags();
                    self.preview_supersede();
                    self.dirty = Dirty::Full;
                }
            }
            if let Some((_, at)) = &self.msg {
                if at.elapsed() > Duration::from_secs(3) {
                    self.msg = None;
                    self.dirty = self.dirty.max(Dirty::TwoLines);
                }
            }
        }
        // 4. presel
        if let Some(a) = self.pending.take() {
            return self.apply(a);
        }
        None
    }

    fn handle_event(&mut self, ev: Event) -> Option<Flow> {
        match ev {
            Event::Key(k) => {
                // Windows ConPTY emits Release/Repeat too — only Press fires.
                if k.kind != KeyEventKind::Press {
                    return None;
                }
                let action = self.key_action(k);
                self.apply(action)
            }
            Event::Mouse(m) => {
                let a = self.mouse_action(m);
                self.apply(a)
            }
            Event::Resize(w, h) => {
                self.screen.resize(w, h);
                self.dirty = Dirty::Full;
                self.preview_supersede(); // pane size is in the sig
                None
            }
            _ => None,
        }
    }

    /// Mode-aware key routing. Text-entry modes (filter, prompts) turn
    /// printable keys into `Input` edits before the keymap is consulted;
    /// confirm dialogs accept only y/Y. ESC vs Alt needs no probing here:
    /// crossterm already folds ESC+key into one ALT-modified event.
    fn key_action(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if k.code == KeyCode::Esc {
            return Action::Esc;
        }
        match self.mode {
            Mode::Confirm(_) => {
                return match k.code {
                    KeyCode::Char('y' | 'Y') if !ctrl => Action::ConfirmYes,
                    _ => Action::Esc, // anything else cancels
                };
            }
            Mode::Filter | Mode::Prompt(_) => {
                if let Some(ev) = input_event(k) {
                    return Action::Input(ev);
                }
                if k.code == KeyCode::Enter {
                    return Action::Open;
                }
                // Fall through: arrows/PgUp/PgDn/C-q keep their bindings.
            }
            _ => {}
        }
        // Alt+key and other unbound keys: harmless no-op.
        self.keymap.get(&k).cloned().unwrap_or(Action::Resize)
    }

    fn mouse_action(&mut self, m: MouseEvent) -> Action {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                let double = self
                    .last_click
                    .take_if(|(r, c, t)| *r == m.row && *c == m.column && now - *t < Duration::from_millis(400));
                if double.is_some() {
                    Action::Open
                } else {
                    self.last_click = Some((m.row, m.column, now));
                    Action::Click { row: m.row, col: m.column, kind: ClickKind::Single }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                Action::Click { row: m.row, col: m.column, kind: ClickKind::Right }
            }
            MouseEventKind::ScrollUp => Action::Click { row: m.row, col: m.column, kind: ClickKind::WheelUp },
            MouseEventKind::ScrollDown => Action::Click { row: m.row, col: m.column, kind: ClickKind::WheelDown },
            _ => Action::Resize, // Drag/Up/Moved: ignored; Resize = no-op action
        }
    }

    // ------------------------------------------------------------ dispatch

    fn apply(&mut self, action: Action) -> Option<Flow> {
        match self.mode.clone() {
            Mode::Normal => self.apply_normal(action),
            Mode::Filter => self.apply_filter_mode(action),
            Mode::Prompt(kind) => self.apply_prompt(kind, action),
            Mode::Confirm(kind) => self.apply_confirm(kind, action),
            Mode::Menu(i) => self.apply_menu(i, action),
            Mode::Pager { lines, title, scroll } => self.apply_pager(lines, title, scroll, action),
        }
    }

    fn apply_normal(&mut self, action: Action) -> Option<Flow> {
        self.dirty = Dirty::None;
        let rows = self.screen.list_rows() as isize;
        match action {
            Action::Next => self.move_cursor(1),
            Action::Prev => self.move_cursor(-1),
            Action::PgDn => self.move_cursor(rows),
            Action::PgUp => self.move_cursor(-rows),
            Action::HalfPgDn => self.move_cursor(rows / 2),
            Action::HalfPgUp => self.move_cursor(-(rows / 2)),
            Action::Home => self.set_cursor(0),
            Action::End => self.set_cursor(self.dir.nvisible().saturating_sub(1)),
            Action::FirstFile => {
                self.dir.jump_first_file();
                self.dirty = Dirty::Full;
            }
            Action::Jump => {
                self.input = InputLine::new("jump ±rows: ");
                self.mode = Mode::Prompt(PromptKind::Jump);
                self.dirty = Dirty::Full;
            }
            Action::Youngest => {
                if let Some(i) = self.dir.entries[..self.dir.nvisible()]
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, e)| e.mtime)
                    .map(|(i, _)| i)
                {
                    self.set_cursor(i);
                }
            }
            Action::Back => self.cd_parent(),
            Action::NavIn => self.nav_in(),
            Action::Open => {
                if let Some(flow) = self.open_hovered() {
                    return Some(flow);
                }
            }
            Action::CdHome => {
                if let Some(h) = self.home.clone() {
                    self.cd(&h);
                }
            }
            Action::CdStart => self.cd(&self.start_dir.clone()),
            Action::CdLast => {
                let target = self.ctx[self.ctx_i].last_dir.clone();
                match target {
                    Some(d) => self.cd(&d),
                    None => self.notify("no previous directory"),
                }
            }
            Action::CdRoot => self.cd(&PathBuf::from("/")),
            Action::CycleCtx(d) => self.cycle_ctx(d),
            Action::GotoCtx(i) => self.goto_ctx(i),
            Action::QuitCtx => return Some(self.quit_ctx()),
            Action::Quit => return Some(Flow::Quit(0)),
            Action::QuitErr => return Some(Flow::Quit(1)),
            Action::QuitCd => {
                self.quit_cd = Some(self.dir.path.clone());
                return Some(Flow::Quit(0));
            }
            Action::ToggleHidden => {
                self.ctx[self.ctx_i].show_hidden = !self.ctx[self.ctx_i].show_hidden;
                self.reload_dir();
            }
            Action::ToggleDetail => {
                self.detail = !self.detail;
                self.dirty = Dirty::Full;
            }
            Action::FileInfo => self.file_info(),
            Action::CycleSort => {
                let key = {
                    let ctx = &mut self.ctx[self.ctx_i];
                    ctx.sort = ctx.sort.next();
                    ctx.sort
                };
                self.apply_ctx_sort_and_filter();
                self.notify(format!("sort: {}", key.label()));
            }
            Action::ReverseSort => {
                self.ctx[self.ctx_i].reverse = !self.ctx[self.ctx_i].reverse;
                self.apply_ctx_sort_and_filter();
            }
            Action::CycleTime => {
                self.cfg.ui.time_type = self.cfg.ui.time_type.next();
                self.reload_dir();
                self.notify(format!("time: {}", self.cfg.ui.time_type.label()));
            }
            Action::Redraw => self.dirty = Dirty::Full,
            Action::TogglePreview => {
                // With a plugin pinned, P closes the pane outright.
                self.preview.enabled = self.preview_pin.take().is_none() && !self.preview.enabled;
                if self.preview.enabled {
                    self.preview_on_hover(); // show the current entry right away
                } else {
                    self.preview.disable();
                }
                self.dirty = Dirty::Full;
            }
            Action::AutoNext => {
                self.auto_next = !self.auto_next;
                self.notify(format!("auto-advance {}", on_off(self.auto_next)));
            }
            Action::Help => {
                let lines = help_lines(&self.cfg.plugins);
                self.mode = Mode::Pager { lines, title: "help — q to close".into(), scroll: 0 };
                self.dirty = Dirty::Full;
            }
            Action::Filter => {
                let ctx = &self.ctx[self.ctx_i];
                let prompt = format!("/{}: ", ctx.filter_mode.label());
                self.input = InputLine::new(&prompt);
                self.input.value = ctx.filter_text.as_deref().unwrap_or("").to_string();
                self.input.cur = self.input.value.chars().count();
                self.mode = Mode::Filter;
                self.dirty = Dirty::Full;
            }
            Action::CycleFilterMode => {
                let mode = {
                    let ctx = &mut self.ctx[self.ctx_i];
                    ctx.filter_mode = ctx.filter_mode.next();
                    ctx.filter_text = None;
                    ctx.filter_mode
                };
                self.apply_ctx_sort_and_filter();
                self.notify(format!("filter: {}", mode.label()));
            }
            Action::Select => self.select_toggle(),
            Action::SelectMul => self.select_range(),
            Action::SelectAll => {
                let paths: Vec<PathBuf> =
                    (0..self.dir.nvisible()).filter_map(|i| self.dir.path_at(i)).collect();
                self.sel.select_all(paths.into_iter());
                self.sync_selection_flags();
                self.persist_sel();
                self.dirty = Dirty::Full;
                self.notify(format!("selected {} items", self.sel.len()));
            }
            Action::SelectInvert => {
                let paths: Vec<PathBuf> =
                    (0..self.dir.nvisible()).filter_map(|i| self.dir.path_at(i)).collect();
                self.sel.invert(paths.into_iter());
                self.sync_selection_flags();
                self.persist_sel();
                self.dirty = Dirty::Full;
            }
            Action::SelEdit => self.sel_edit(),
            Action::SelSize => self.sel_size(),
            Action::Copy | Action::Move => self.paste(if action == Action::Copy { OpKind::Copy } else { OpKind::Move }),
            Action::CopyAs => {
                let src = self.op_source();
                match src {
                    Some(p) => {
                        self.input = InputLine::new("copy as: ");
                        self.input.value = file_name_of(&p);
                        self.input.cur = self.input.value.chars().count();
                        self.mode = Mode::Prompt(PromptKind::CopyAs(p));
                        self.dirty = Dirty::Full;
                    }
                    None => self.notify("nothing to copy"),
                }
            }
            Action::Trash => self.confirm_trash(),
            Action::Delete => self.confirm_delete(),
            Action::New => {
                self.input = InputLine::new("new (dir: trailing /): ");
                self.mode = Mode::Prompt(PromptKind::New);
                self.dirty = Dirty::Full;
            }
            Action::Rename => {
                let path = self.dir.hovered_path()?;
                self.input = InputLine::new("rename: ");
                self.input.value = file_name_of(&path);
                self.input.cur = self.input.value.chars().count();
                self.mode = Mode::Prompt(PromptKind::Rename(path));
                self.dirty = Dirty::Full;
            }
            Action::OpenWith => {
                let path = self.dir.hovered_path()?;
                self.input = InputLine::new("open with: ");
                self.mode = Mode::Prompt(PromptKind::OpenWith(path));
                self.dirty = Dirty::Full;
            }
            Action::Edit => self.external_edit(),
            Action::Shell => self.suspend_and_run(&plugin::shell(), vec![]),
            Action::PluginMenu => self.open_menu(),
            Action::PluginRun(i) => self.run_plugin(i),
            Action::Click { row, col, kind } => self.on_click(row, col, kind),
            Action::Resize | Action::ConfirmYes => {}
            Action::Esc => {
                // Esc: cancel a running file op (between files), else unpin a
                // plugin preview, else drop the accepted filter, else clear a
                // pending range anchor.
                if let Some(op) = self.op.as_ref() {
                    op.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.notify("cancelling…");
                } else if self.preview_pin.is_some() {
                    self.unpin_preview();
                } else if self.ctx[self.ctx_i].filter_text.is_some() {
                    self.ctx[self.ctx_i].filter_text = None;
                    self.apply_ctx_sort_and_filter();
                    self.msg = None;
                } else if self.sel.range_anchor.take().is_some() {
                    self.notify("range cancelled");
                }
            }
            Action::Input(_) => {}
        }
        None
    }

    fn apply_filter_mode(&mut self, action: Action) -> Option<Flow> {
        self.dirty = Dirty::Full;
        match action {
            Action::Input(InputEv::Char(c)) => {
                self.input.insert(c);
                self.live_filter();
            }
            Action::Input(InputEv::Backspace) => {
                self.input.backspace();
                self.live_filter();
            }
            Action::Input(ev) => match ev {
                InputEv::Left => self.input.left(),
                InputEv::Right => self.input.right(),
                InputEv::Home => self.input.home(),
                InputEv::End => self.input.end(),
                InputEv::WordBack => {
                    self.input.word_back();
                    self.live_filter();
                }
                InputEv::Clear => {
                    self.input.clear();
                    self.live_filter();
                }
                InputEv::Delete => self.input.delete(),
                // Handled by the arms above.
                InputEv::Char(_) | InputEv::Backspace => {}
            },
            Action::Esc => {
                // Esc in filter = drop the filter entirely (nnn semantics).
                self.ctx[self.ctx_i].filter_text = None;
                self.input.clear();
                self.mode = Mode::Normal;
                self.apply_ctx_sort_and_filter();
            }
            Action::Open => {
                // Enter accepts the filter and returns to browsing.
                let text = self.input.value.trim().to_string();
                self.ctx[self.ctx_i].filter_text =
                    if text.is_empty() { None } else { Some(text.into_boxed_str()) };
                self.mode = Mode::Normal;
                self.apply_ctx_sort_and_filter();
                let n = self.dir.nvisible();
                self.notify(format!("{n} matches"));
            }
            // Keep navigating while filtering (nnn allows arrows mid-filter).
            Action::Next => self.move_cursor(1),
            Action::Prev => self.move_cursor(-1),
            Action::PgDn => self.move_cursor(self.screen.list_rows() as isize),
            Action::PgUp => self.move_cursor(-(self.screen.list_rows() as isize)),
            Action::Home => self.set_cursor(0),
            Action::End => self.set_cursor(self.dir.nvisible().saturating_sub(1)),
            Action::Click { row, .. } => self.on_click(row, 0, ClickKind::Single),
            Action::Quit => return Some(Flow::Quit(0)),
            _ => {}
        }
        None
    }

    fn apply_prompt(&mut self, kind: PromptKind, action: Action) -> Option<Flow> {
        self.dirty = Dirty::Full;
        match action {
            Action::Input(InputEv::Char(c)) => self.input.insert(c),
            Action::Input(InputEv::Backspace) => self.input.backspace(),
            Action::Input(InputEv::Delete) => self.input.delete(),
            Action::Input(InputEv::Left) => self.input.left(),
            Action::Input(InputEv::Right) => self.input.right(),
            Action::Input(InputEv::Home) => self.input.home(),
            Action::Input(InputEv::End) => self.input.end(),
            Action::Input(InputEv::WordBack) => self.input.word_back(),
            Action::Input(InputEv::Clear) => self.input.clear(),
            Action::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            Action::Open => {
                let value = self.input.value.trim().to_string();
                self.input.clear();
                self.mode = Mode::Normal;
                if value.is_empty() {
                    return None;
                }
                self.submit_prompt(kind, value);
            }
            _ => {}
        }
        None
    }

    fn submit_prompt(&mut self, kind: PromptKind, value: String) {
        match kind {
            PromptKind::Jump => {
                let delta: isize = value.parse().unwrap_or(0);
                self.move_cursor(delta);
            }
            PromptKind::New => {
                let path = self.dir.path.join(&value);
                let res = if value.ends_with('/') {
                    std::fs::create_dir_all(&path)
                } else {
                    std::fs::File::create(&path).map(|_| ())
                };
                match res {
                    Ok(()) => self.reload_dir(),
                    Err(e) => self.notify(format!("create: {e}")),
                }
            }
            PromptKind::Rename(old) => {
                let new = self.dir.path.join(&value);
                match std::fs::rename(&old, &new) {
                    Ok(()) => {
                        let name = file_name_of(&new);
                        self.reload_dir();
                        self.dir.move_to_name(&name);
                    }
                    Err(e) => self.notify(format!("rename: {e}")),
                }
            }
            PromptKind::CopyAs(src) => {
                let req = OpRequest {
                    kind: OpKind::Copy,
                    items: vec![src],
                    dest: Some(self.dir.path.clone()),
                    new_names: Some(vec![value]),
                };
                self.op = Some(ops::start(req));
                self.notify("copying…");
            }
            PromptKind::OpenWith(target) => {
                let cmdline = format!("{value} {}", shquote(&target.to_string_lossy()));
                self.suspend_and_run(&cmdline, vec![]);
            }
        }
    }

    fn apply_confirm(&mut self, kind: ConfirmKind, action: Action) -> Option<Flow> {
        self.dirty = Dirty::Full;
        self.mode = Mode::Normal;
        match action {
            Action::ConfirmYes => self.start_op_from_confirm(kind),
            _ => self.notify("cancelled"),
        }
        None
    }

    fn start_op_from_confirm(&mut self, kind: ConfirmKind) {
        let (kind, items) = match kind {
            ConfirmKind::Trash(items) => (OpKind::Trash, items),
            ConfirmKind::Delete(items) => (OpKind::Delete, items),
        };
        let req = OpRequest { kind, items, dest: None, new_names: None };
        self.op = Some(ops::start(req));
        self.notify("working…");
    }

    fn apply_menu(&mut self, hl: usize, action: Action) -> Option<Flow> {
        self.dirty = Dirty::Full;
        match action {
            Action::Next | Action::Prev => {
                let n = self.menu_items.len();
                if n > 0 {
                    let next = if action == Action::Next {
                        (hl + 1).min(n - 1)
                    } else {
                        hl.saturating_sub(1)
                    };
                    self.mode = Mode::Menu(next);
                }
            }
            Action::Open => {
                let idx = self.menu_items.get(hl).copied();
                self.mode = Mode::Normal;
                if let Some(i) = idx {
                    return self.apply(Action::PluginRun(i));
                }
            }
            Action::Esc | Action::QuitCtx => self.mode = Mode::Normal,
            _ => {}
        }
        None
    }

    fn apply_pager(
        &mut self,
        lines: Vec<Line>,
        title: String,
        scroll: usize,
        action: Action,
    ) -> Option<Flow> {
        self.dirty = Dirty::Full;
        // The body spans rows 1..=list_rows+1 (see render_pager); stop
        // scrolling once the last line reaches the bottom.
        let page = self.screen.list_rows() + 1;
        let rows = page.saturating_sub(1).max(1);
        let max = lines.len().saturating_sub(page);
        let new_scroll = match action {
            Action::Next => (scroll + 1).min(max),
            Action::Prev => scroll.saturating_sub(1),
            Action::PgDn | Action::HalfPgDn => (scroll + rows).min(max),
            Action::PgUp | Action::HalfPgUp => scroll.saturating_sub(rows),
            Action::Home => 0,
            Action::End => max,
            Action::Esc | Action::QuitCtx | Action::Help => {
                self.mode = Mode::Normal;
                return None;
            }
            Action::Quit => return Some(Flow::Quit(0)),
            _ => scroll,
        };
        self.mode = Mode::Pager { lines, title, scroll: new_scroll };
        None
    }

    // ------------------------------------------------------------ helpers

    fn move_cursor(&mut self, delta: isize) {
        let before = self.dir.cursor;
        if self.cfg.ui.wrap_nav {
            self.dir.move_cursor(delta);
        } else {
            let n = self.dir.nvisible();
            if n > 0 {
                self.dir.cursor = (self.dir.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
            }
        }
        self.after_cursor_move(before);
    }

    fn set_cursor(&mut self, i: usize) {
        let before = self.dir.cursor;
        let n = self.dir.nvisible();
        self.dir.cursor = i.min(n.saturating_sub(1));
        self.after_cursor_move(before);
    }

    fn after_cursor_move(&mut self, before: usize) {
        if self.dir.cursor == before {
            return;
        }
        self.unpin_preview(); // navigating hands the pane back to the hover preview
        // render() downgrades TwoLines to a full frame if the scroll window
        // moved. The preview pane tracks hover, so it always needs a full one.
        if self.preview.enabled {
            self.dirty = Dirty::Full;
            self.preview_on_hover();
        } else {
            self.dirty = self.dirty.max(Dirty::TwoLines);
        }
    }

    fn cd(&mut self, path: &Path) {
        // Relative targets (plugin `vfm:cd sub`) are relative to the browsed
        // directory, not to vfm's process cwd.
        let path = match std::fs::canonicalize(self.dir.path.join(path)) {
            Ok(p) => p,
            Err(e) => {
                self.notify(format!("cd: {e}"));
                return;
            }
        };
        if !path.is_dir() {
            self.notify("not a directory");
            return;
        }
        // Remember where we were for '-' and context restore.
        self.ctx[self.ctx_i].last_dir = Some(self.dir.path.clone());
        self.ctx[self.ctx_i].last_name = self.dir.hovered_name().map(str::to_string).map(Into::into);
        self.load_dir_at(path);
        self.dirty = Dirty::Full;
    }

    /// Go to the parent directory (with cursor restore). Do nothing at the root.
    fn cd_parent(&mut self) {
        let parent = self.dir.path.parent().map(Path::to_path_buf);
        match parent {
            Some(p) if p != self.dir.path => {
                let name = file_name_of(&self.dir.path.clone());
                self.cd(&p);
                self.dir.move_to_name(&name);
            }
            _ => {}
        }
    }

    fn nav_in(&mut self) {
        let is_dir = self.dir.entries.get(self.dir.cursor).is_some_and(|e| e.is_dir());
        if is_dir {
            if let Some(p) = self.dir.hovered_path() {
                self.cd(&p);
            }
        }
    }

    fn open_hovered(&mut self) -> Option<Flow> {
        let (path, name, is_dir, is_exec) = {
            let e = self.dir.entries.get(self.dir.cursor)?;
            (self.dir.path_at(self.dir.cursor)?, e.name.clone(), e.is_dir(), e.flags.has(EntryFlags::EXEC))
        };
        if is_dir {
            self.cd(&path);
            return None;
        }
        // Files: executables run in-place; the rest is `file_open`'s call.
        if is_exec {
            let file = file_name_of(&path);
            self.suspend_and_run(&format!("./{}", shquote(&file)), vec![]);
            return None;
        }
        match self.file_open(&path, &name, plugin::which("kitim")) {
            FileOpen::Program(cmdline) => self.suspend_and_run(&cmdline, vec![]),
            FileOpen::Edit => self.external_edit(),
            FileOpen::Nothing => {}
        }
        None
    }

    /// What Enter does with a hovered file.
    ///
    /// A matching `[[opener]]` rule runs fullscreen, first match winning.
    /// Otherwise kitim plays images, video and audio — `kitim_installed` is
    /// passed in so tests decide it — but only on a terminal that answered
    /// the graphics probe, since kitim would show nothing without it. What's
    /// left opens in the editor if its head reads as text, else nothing: no
    /// more vi over binary bytes.
    fn file_open(&self, path: &Path, name: &str, kitim_installed: bool) -> FileOpen {
        if let Some(rule) = self
            .cfg
            .openers
            .iter()
            .find(|r| r.patterns.iter().any(|p| crate::util::glob_match(p.as_bytes(), name.as_bytes())))
        {
            return FileOpen::Program(format!("{} {}", rule.cmd.trim(), shquote(&file_name_of(path))));
        }
        if self.graphics && kitim_installed && crate::config::is_kitim_media(name) {
            // --fullscreen: images fill the screen and wait for a key, so
            // the viewer doesn't flash and vanish.
            let file = shquote(&file_name_of(path));
            return FileOpen::Program(format!("kitim --fullscreen {file}"));
        }
        if crate::preview::opens_as_text(path) {
            FileOpen::Edit
        } else {
            FileOpen::Nothing
        }
    }

    /// Suspend the TUI, run a command line in the current dir, resume.
    fn suspend_and_run(&mut self, cmdline: &str, extra_env: Vec<(String, String)>) {
        let dir = self.dir.path.clone();
        let hovered = self.dir.hovered_path();
        let sel_file = (!self.sel.is_empty()).then(|| {
            let _ = self.sel.write_sel();
            self.sel.file.clone()
        });
        let mut env = plugin::child_env(&dir, hovered.as_deref(), sel_file.as_deref(), self.ctx_i as u8);
        env.extend(extra_env);
        // Re-entering the alternate screen wipes images; render() re-sends.
        self.hide_graphics();
        // Stop the previewer meanwhile: a video would keep playing its sound.
        self.preview.disable();
        self.screen.flush();
        self.screen.leave();
        let res = plugin::run_fullscreen(cmdline, &dir, &env);
        let _ = self.screen.enter();
        self.dirty = Dirty::Full;
        // The child may have changed the directory contents.
        if self.dir.changed() {
            self.reload_dir();
        } else {
            self.preview_on_hover();
        }
        match res {
            Ok(()) => {}
            Err(e) => self.notify(format!("run: {e}")),
        }
    }

    fn external_edit(&mut self) {
        let Some(path) = self.dir.hovered_path() else { return };
        let editor = plugin::editor();
        let name = file_name_of(&path);
        self.suspend_and_run(&format!("{} {}", editor, shquote(&name)), vec![]);
    }

    fn reload_dir(&mut self) {
        let path = self.dir.path.clone();
        let tt = self.cfg.ui.time_type;
        let hidden = self.ctx[self.ctx_i].show_hidden;
        match Dir::load(&path, tt, hidden) {
            Ok(mut d) => {
                d.sort = self.ctx[self.ctx_i].sort;
                d.reverse = self.ctx[self.ctx_i].reverse;
                let hovered = self.dir.hovered_name().map(str::to_string);
                d.ensure_sorted();
                self.dir = d;
                self.apply_ctx_sort_and_filter();
                if let Some(h) = hovered {
                    self.dir.move_to_name(&h);
                }
                self.sync_selection_flags();
                self.preview_supersede();
                self.dirty = Dirty::Full;
            }
            Err(e) => self.notify(format!("reload: {e}")),
        }
    }

    fn load_dir_at(&mut self, path: PathBuf) {
        self.unpin_preview();
        let tt = self.cfg.ui.time_type;
        let hidden = self.ctx[self.ctx_i].show_hidden;
        match Dir::load(&path, tt, hidden) {
            Ok(mut d) => {
                d.sort = self.ctx[self.ctx_i].sort;
                d.reverse = self.ctx[self.ctx_i].reverse;
                self.dir = d;
                if let Some(name) = self.ctx[self.ctx_i].last_name.clone() {
                    self.dir.move_to_name(&name);
                }
                self.apply_ctx_sort_and_filter();
                self.sync_selection_flags();
                self.preview_supersede();
                self.dirty = Dirty::Full;
            }
            Err(e) => self.notify(format!("cd: {e}")),
        }
    }

    /// Re-apply the context's sort keys + saved filter to the live Dir.
    fn apply_ctx_sort_and_filter(&mut self) {
        let ctx = self.ctx[self.ctx_i].clone();
        self.dir.sort = ctx.sort;
        self.dir.reverse = ctx.reverse;
        self.dir.bump();
        self.dir.ensure_sorted();
        let matcher = ctx.matcher();
        let prev = self.dir.hovered_name().map(str::to_string);
        self.dir.apply_filter(matcher.as_ref(), ctx.filter_mode == FilterMode::Fuzzy, prev.as_deref());
        self.dirty = Dirty::Full;
    }

    fn live_filter(&mut self) {
        let text = self.input.value.clone();
        let mode = self.ctx[self.ctx_i].filter_mode;
        let matcher = if text.is_empty() {
            None
        } else {
            Matcher::new(&text, mode, self.cfg.filter.smartcase).ok()
        };
        let prev = self.dir.hovered_name().map(str::to_string);
        self.dir.apply_filter(matcher.as_ref(), mode == FilterMode::Fuzzy, prev.as_deref());
        self.dirty = Dirty::Full;
    }

    // ---- contexts

    fn save_ctx(&mut self) {
        let c = &mut self.ctx[self.ctx_i];
        c.path = self.dir.path.clone();
        c.last_name = self.dir.hovered_name().map(str::to_string).map(Into::into);
    }

    fn ctx_open(&self, i: usize) -> bool {
        self.ctx_active & (1 << i) != 0
    }

    fn goto_ctx(&mut self, i: u8) {
        let i = i as usize;
        if i == self.ctx_i || i >= CTX_MAX {
            return;
        }
        self.save_ctx();
        if !self.ctx_open(i) {
            // Opening a closed slot: fresh context in the current directory
            // (nnn behavior); its old state is discarded.
            self.ctx[i] = Context::new(self.dir.path.clone(), &self.cfg);
            self.ctx_active |= 1 << i;
        }
        self.ctx_i = i;
        let target = self.ctx[i].path.clone();
        self.load_dir_at(target);
    }

    /// Tab / S-Tab: next open context in direction `d` (wraps).
    fn cycle_ctx(&mut self, d: i8) {
        let n = CTX_MAX as isize;
        let next = (1..n)
            .map(|k| (self.ctx_i as isize + k * d.signum() as isize).rem_euclid(n) as usize)
            .find(|&j| self.ctx_open(j));
        match next {
            Some(j) => self.goto_ctx(j as u8),
            None => self.notify("no other context open (1-8 opens one)"),
        }
    }

    /// q: close this context; the last one closing quits vfm.
    fn quit_ctx(&mut self) -> Flow {
        let i = self.ctx_i;
        let next = (1..CTX_MAX).map(|k| (i + k) % CTX_MAX).find(|&j| self.ctx_open(j));
        let Some(j) = next else { return Flow::Quit(0) };
        self.ctx_active &= !(1 << i);
        self.ctx_i = j;
        let p = self.ctx[j].path.clone();
        self.load_dir_at(p);
        Flow::Continue
    }

    // ---- selection

    /// Re-stamp SELECTED flags from the selection set after any listing change.
    fn sync_selection_flags(&mut self) {
        for i in 0..self.dir.entries.len() {
            let selected = self
                .dir
                .path_at(i)
                .map(|p| self.sel.contains(&p))
                .unwrap_or(false);
            let flags = &mut self.dir.entries[i].flags;
            if selected {
                flags.set(EntryFlags::SELECTED);
            } else {
                flags.discard(EntryFlags::SELECTED);
            }
        }
    }

    fn persist_sel(&mut self) {
        if let Err(e) = self.sel.write_sel() {
            self.notify(format!("selection file: {e}"));
        }
    }

    fn select_toggle(&mut self) {
        let Some(path) = self.dir.hovered_path() else { return };
        let now_sel = self.sel.toggle(&path);
        let i = self.dir.cursor;
        if now_sel {
            self.dir.entries[i].flags.set(EntryFlags::SELECTED);
        } else {
            self.dir.entries[i].flags.discard(EntryFlags::SELECTED);
        }
        self.persist_sel();
        self.move_cursor(1); // nnn advances after toggle
        self.dirty = Dirty::Full;
    }

    fn select_range(&mut self) {
        let anchor = self.sel.range_anchor;
        match anchor {
            None => {
                self.sel.range_anchor = Some(self.dir.cursor);
                self.notify("range: start (m again to complete)");
            }
            Some(a) => {
                let (lo, hi) = (a.min(self.dir.cursor), a.max(self.dir.cursor));
                for i in lo..=hi {
                    if let Some(p) = self.dir.path_at(i) {
                        self.sel.add(&p);
                        self.dir.entries[i].flags.set(EntryFlags::SELECTED);
                    }
                }
                self.sel.range_anchor = None;
                self.persist_sel();
                self.notify(format!("range: {} items", hi - lo + 1));
                self.dirty = Dirty::Full;
            }
        }
    }

    fn op_source(&self) -> Option<PathBuf> {
        if !self.sel.is_empty() {
            self.sel.items().first().cloned()
        } else {
            self.dir.hovered_path()
        }
    }

    fn paste(&mut self, kind: OpKind) {
        if self.sel.is_empty() {
            self.notify("nothing selected");
            return;
        }
        let items: Vec<PathBuf> = self
            .sel
            .items()
            .iter()
            .filter(|p| Some(p.as_path()) != Some(self.dir.path.as_path()))
            .cloned()
            .collect();
        if items.is_empty() {
            self.notify("nothing to paste");
            return;
        }
        let verb = kind.verb();
        let req =
            OpRequest { kind, items, dest: Some(self.dir.path.clone()), new_names: None };
        self.op = Some(ops::start(req));
        self.notify(format!("{verb}…"));
        self.dirty = Dirty::Full;
    }

    fn confirm_trash(&mut self) {
        let items = self.op_targets();
        if items.is_empty() {
            self.notify("nothing to trash");
            return;
        }
        self.mode = Mode::Confirm(ConfirmKind::Trash(items));
        self.dirty = Dirty::Full;
    }

    fn confirm_delete(&mut self) {
        let items = self.op_targets();
        if items.is_empty() {
            self.notify("nothing to delete");
            return;
        }
        self.mode = Mode::Confirm(ConfirmKind::Delete(items));
        self.dirty = Dirty::Full;
    }

    /// Selection if any, else the hovered entry — what file ops act on.
    fn op_targets(&self) -> Vec<PathBuf> {
        if !self.sel.is_empty() {
            self.sel.items().to_vec()
        } else {
            self.dir.hovered_path().into_iter().collect()
        }
    }

    fn finish_op(&mut self, res: ops::OpResult) {
        let verb = res.kind.verb().trim_end_matches("ing").to_string();
        if res.cancelled {
            self.notify(format!("{verb} cancelled ({} done)", res.done.len()));
        } else if res.errors.is_empty() {
            self.notify(format!("{verb}ed {} item(s)", res.done.len()));
        } else {
            let first = &res.errors[0].1;
            self.notify(format!("{verb}: {} errors ({}), {} ok", res.errors.len(), first, res.done.len()));
        }
        self.reload_dir();
        self.sel.prune_missing();
        self.sync_selection_flags();
        self.persist_sel();
        self.dirty = Dirty::Full;
    }

    fn sel_edit(&mut self) {
        let f = self.sel.file.clone();
        let _ = self.sel.write_sel();
        let dir = self.dir.path.clone();
        let cmdline = format!("{} {}", plugin::editor(), shquote(&f.to_string_lossy()));
        self.screen.leave();
        let res = plugin::run_fullscreen(&cmdline, &dir, &[]);
        let _ = self.screen.enter();
        match res {
            Ok(()) => match self.sel.reload_from_disk() {
                Ok(n) => {
                    self.sync_selection_flags();
                    self.persist_sel();
                    self.dirty = Dirty::Full;
                    self.notify(format!("selection: {n} items"));
                }
                Err(e) => self.notify(format!("selection: {e}")),
            },
            Err(e) => self.notify(format!("editor: {e}")),
        }
        self.dirty = Dirty::Full;
    }

    fn sel_size(&mut self) {
        let mut total = 0u64;
        let mut missing = 0usize;
        for p in self.sel.items() {
            match std::fs::symlink_metadata(p) {
                Ok(m) => {
                    if m.is_dir() {
                        // Shallow: file manager shouldn't du-walk uninvited.
                        total += m.len();
                    } else {
                        total += m.len();
                    }
                }
                Err(_) => missing += 1,
            }
        }
        let n = self.sel.len();
        self.notify(format!("{n} selected, {} total{}", coolsize(total),
            if missing > 0 { format!(", {missing} missing") } else { String::new() }));
    }

    fn file_info(&mut self) {
        let Some(e) = self.dir.entries.get(self.dir.cursor) else { return };
        let size = if e.is_dir() { "dir".to_string() } else { coolsize(e.size).to_string() };
        self.notify(format!(
            "{} {} {} mtime {}",
            octal3(e.mode),
            size,
            e.name,
            fmt_time(e.mtime, now_secs())
        ));
    }

    // ---- preview plumbing

    fn preview_supersede(&mut self) {
        // Dir contents changed: the old sig can't match anymore — restart.
        if self.preview.enabled {
            self.preview.disable();
            self.preview_on_hover();
        }
    }

    /// The terminal is big enough for a side pane.
    fn pane_fits(&self) -> bool {
        self.screen.w >= 100 && self.screen.h >= 10
    }

    fn preview_pane(&self) -> Option<(u16, u16)> {
        let (w, h) = (self.screen.w, self.screen.h);
        (self.preview.enabled && self.pane_fits()).then(|| (w / 2 - 1, h.saturating_sub(4)))
    }

    fn preview_on_hover(&mut self) {
        if !self.preview.enabled {
            return;
        }
        let Some((pw, _ph)) = self.preview_pane() else { return };
        if let Some(pin) = self.preview_pin {
            self.request_pinned(pin.plugin, pw);
            return;
        }
        let Some(e) = self.dir.entries.get(self.dir.cursor) else {
            self.preview.disable();
            return;
        };
        let Some(path) = self.dir.hovered_path() else { return };
        let rule = self.match_previewer(&e.name);
        let sig = make_sig(&path, e.size, e.mtime, (pw, self.screen.h), rule.unwrap_or(usize::MAX));
        let mut pane = (pw, self.screen.h);
        let source = match rule.map(|i| &self.cfg.previewers[i]) {
            Some(r) => {
                let argv = crate::util::shlex(&r.cmd).unwrap_or_else(|| vec![r.cmd.clone()]);
                if r.graphics {
                    // Exactly the body: an image may fill every cell it's given.
                    pane = (pw - 1, self.screen.list_rows() as u16);
                    Source::Graphics(argv)
                } else {
                    Source::Previewer(argv)
                }
            }
            None => Source::Builtin,
        };
        let env = plugin::child_env(
            &self.dir.path,
            Some(&path),
            (!self.sel.is_empty()).then(|| self.sel.file.clone()).as_deref(),
            self.ctx_i as u8,
        );
        self.preview.set_context(pane, env, &self.dir.path);
        self.preview.request(Target { path, source, sig }, Instant::now());
    }

    /// (Re)run a pinned `>` plugin for the current directory. Also called on
    /// resize and directory refresh, so the output stays current.
    fn request_pinned(&mut self, plugin: usize, pane_w: u16) {
        let Some(def) = self.cfg.plugins.get(plugin) else { return };
        let (_, cmdline) = plugin::resolve_mode(&def.cmd);
        let source = Source::Shell(cmdline.to_string());
        let pane = (pane_w, self.screen.h);
        // Distinct from any hover sig (rule ids are < previewers.len()).
        let sig = make_sig(&self.dir.path, plugin as u64, 0, pane, usize::MAX - 1);
        let hovered = self.dir.hovered_path();
        let sel = (!self.sel.is_empty()).then(|| self.sel.file.clone());
        let env = plugin::child_env(&self.dir.path, hovered.as_deref(), sel.as_deref(), self.ctx_i as u8);
        self.preview.set_context(pane, env, &self.dir.path);
        self.preview.request(Target { path: self.dir.path.clone(), source, sig }, Instant::now());
    }

    /// Show plugin `i`'s output in the preview pane, opening it if needed.
    /// Pressing the same plugin key again closes it (toggle).
    fn pin_preview(&mut self, i: usize) {
        if self.preview_pin.is_some_and(|p| p.plugin == i) {
            self.unpin_preview();
            return;
        }
        let was_enabled = self.preview_pin.map_or(self.preview.enabled, |p| p.was_enabled);
        self.preview_pin = Some(Pin { plugin: i, was_enabled });
        self.preview.enabled = true;
        self.preview.disable(); // drop the hover preview's state; never shown stale
        self.preview_on_hover();
        self.dirty = Dirty::Full;
    }

    /// Hand the pane back: hover preview if it was on before, else close it.
    fn unpin_preview(&mut self) {
        let Some(pin) = self.preview_pin.take() else { return };
        self.preview.disable();
        self.preview.enabled = pin.was_enabled;
        self.preview_on_hover();
        self.dirty = Dirty::Full;
    }

    /// First rule matching `name`; graphics rules only if the terminal can.
    fn match_previewer(&self, name: &str) -> Option<usize> {
        self.cfg.previewers.iter().position(|r| {
            (self.graphics || !r.graphics)
                && r.patterns
                    .iter()
                    .any(|p| crate::util::glob_match(p.as_bytes(), name.as_bytes()))
        })
    }

    // ---- plugins

    fn open_menu(&mut self) {
        if self.cfg.plugins.is_empty() {
            self.notify("no plugins configured ([[plugin]] in config.toml)");
            return;
        }
        self.menu_items = (0..self.cfg.plugins.len()).collect();
        self.mode = Mode::Menu(0);
        self.dirty = Dirty::Full;
    }

    fn run_plugin(&mut self, i: usize) {
        let Some(def) = self.cfg.plugins.get(i).cloned() else { return };
        let (mode, cmdline) = plugin::resolve_mode(&def.cmd);
        if cmdline.is_empty() {
            self.notify("plugin cmd is empty");
            return;
        }
        if let Some(prog) = plugin::missing_requirement(&def.requires) {
            self.notify(format!("{}: needs `{prog}` (not found in PATH)", def.name));
            return;
        }
        // Too narrow for a side pane: show the same output in the pager.
        if mode == SpawnMode::Preview && self.pane_fits() {
            self.pin_preview(i);
            return;
        }
        let dir = self.dir.path.clone();
        let hovered = self.dir.hovered_path();
        let sel_file = (!self.sel.is_empty()).then(|| {
            let _ = self.sel.write_sel();
            self.sel.file.clone()
        });
        let env = plugin::child_env(&dir, hovered.as_deref(), sel_file.as_deref(), self.ctx_i as u8);
        match mode {
            SpawnMode::Fullscreen => self.suspend_and_run(cmdline, env),
            SpawnMode::Capture | SpawnMode::Preview => match plugin::run_capture(cmdline, &dir, &env) {
                Ok(capture) => {
                    self.apply_replies(&capture.replies);
                    // Control lines are protocol, not output: keep them out of
                    // the pager. The rest goes through the ANSI parser like
                    // previews: colors kept, cursor-moving escapes dropped.
                    let mut feeder = crate::ansi::AnsiFeeder::new();
                    for l in capture.output.lines().filter(|l| !l.starts_with(plugin::CTL_PREFIX)) {
                        feeder.feed(l.as_bytes());
                        feeder.feed(b"\n");
                    }
                    feeder.finish();
                    let mut lines = feeder.take_lines();
                    if lines.is_empty() {
                        lines.push(Line::plain("(no output)"));
                    }
                    let mut title = def.name.clone();
                    if !capture.exit_ok {
                        title.push_str(" (nonzero exit)");
                    }
                    self.mode = Mode::Pager { lines, title, scroll: 0 };
                    self.dirty = Dirty::Full;
                }
                Err(e) => self.notify(format!("plugin: {e}")),
            },
        }
    }

    fn apply_replies(&mut self, replies: &[Reply]) {
        for r in replies {
            match r {
                Reply::Cd(p) => self.cd(p),
                Reply::Open(p) => {
                    if p.is_dir() {
                        self.cd(p);
                    } else {
                        let dir = self.dir.path.clone();
                        let cmdline = format!("{} {}", plugin::editor(), shquote(&p.to_string_lossy()));
                        self.screen.leave();
                        let _ = plugin::run_fullscreen(&cmdline, &dir, &[]);
                        let _ = self.screen.enter();
                        self.dirty = Dirty::Full;
                    }
                }
                Reply::Refresh => self.reload_dir(),
                Reply::SelectAdd(p) => {
                    self.sel.add(p);
                    self.sync_selection_flags();
                    self.persist_sel();
                }
                Reply::SelectRemove(p) => {
                    self.sel.remove(p);
                    self.sync_selection_flags();
                    self.persist_sel();
                }
                Reply::SelectClear => {
                    self.sel.clear();
                    self.sync_selection_flags();
                    self.persist_sel();
                }
                Reply::Echo(s) => self.notify(s.clone()),
                Reply::Quit => {}
            }
        }
    }

    // ---- mouse

    fn on_click(&mut self, row: u16, _col: u16, kind: ClickKind) {
        match kind {
            ClickKind::WheelUp => self.move_cursor(-1),
            ClickKind::WheelDown => self.move_cursor(1),
            ClickKind::Right => {
                // Right click: move + toggle-select (quick triage).
                if self.row_to_index(row).is_some() {
                    self.select_toggle();
                }
            }
            ClickKind::Single => {
                if let Some(i) = self.row_to_index(row) {
                    self.set_cursor(i);
                }
            }
        }
    }

    fn row_to_index(&self, row: u16) -> Option<usize> {
        let top = 2u16;
        if row < top {
            return None;
        }
        let i = self.dir.scroll + (row - top) as usize;
        (i < self.dir.nvisible()).then_some(i)
    }

    pub fn notify(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), Instant::now()));
        self.dirty = self.dirty.max(Dirty::TwoLines);
    }
}

// ------------------------------------------------------------- rendering

impl App {
    pub fn render(&mut self) {
        // Op completion is noticed here (between key presses the loop always
        // renders first) so the statusbar flips to the summary immediately.
        let op_done = self.op.as_ref().is_some_and(|o| o.is_done());
        if op_done {
            let handle = self.op.take().expect("op");
            let res = handle.finish();
            self.finish_op(res);
            self.dirty = Dirty::Full;
        }
        if self.dirty == Dirty::TwoLines && self.fast_two_lines_ok() {
            self.fast_two_lines();
        } else if self.dirty != Dirty::None {
            self.render_full();
        }
        self.dirty = Dirty::None;
        // After the text, so the image goes over a freshly cleared pane.
        self.sync_graphics();
        self.screen.flush();
    }

    /// Top-left cell of the pane body while an image may show there: the
    /// pane is open and no overlay (menu, pager) covers it.
    fn graphics_origin(&self) -> Option<(u16, u16)> {
        let (pane_w, _) = self.preview_pane()?;
        if matches!(self.mode, Mode::Menu(_) | Mode::Pager { .. }) {
            return None;
        }
        Some((2, self.screen.w - pane_w + 1))
    }

    /// Puts the preview's image on screen, or takes it off, to match the pane.
    ///
    /// Images sit above the text and survive text redraws, so output is only
    /// needed when the wanted frame changes. A new frame goes up before the
    /// old one is deleted, so animations don't flicker; terminals draw the
    /// newer image id on top. A frame without an id can't be deleted alone,
    /// so every image is cleared before it goes up.
    fn sync_graphics(&mut self) {
        let origin = self.graphics_origin();
        let frame = origin.and(self.preview.image());
        let old = self.shown;
        if old.map(|s| (s.seq, Some(s.origin))) == frame.map(|(seq, _)| (seq, origin)) {
            return;
        }
        let buf = &mut self.screen.buf;
        let (Some(origin), Some((seq, frame))) = (origin, frame) else {
            if let Some(old) = self.shown.take() {
                graphics::delete(buf, old.id);
            }
            return;
        };
        if old.is_some() && frame.id.is_none() {
            graphics::delete(buf, None);
        }
        self.screen.move_to(origin.0, origin.1);
        self.screen.buf.extend_from_slice(&frame.bytes);
        if let Some(old_id) = old.and_then(|s| s.id).filter(|&id| Some(id) != frame.id) {
            graphics::delete(&mut self.screen.buf, Some(old_id));
        }
        self.shown = Some(Shown { seq, origin, id: frame.id });
    }

    /// Takes the preview image off screen (output is queued, not flushed).
    fn hide_graphics(&mut self) {
        if let Some(old) = self.shown.take() {
            graphics::delete(&mut self.screen.buf, old.id);
        }
    }

    /// The fast path applies only when the scroll window stays put. Scroll is
    /// derived (`scroll_adjust`), so derive it now: moving within the
    /// scrolloff band can shift the window even by one row.
    fn fast_two_lines_ok(&mut self) -> bool {
        matches!(self.mode, Mode::Normal | Mode::Filter)
            && self.dir.scroll_adjust(self.screen.list_rows(), self.cfg.ui.scrolloff) == self.last_scroll
    }

    /// Emit just the old + new cursor rows (+ statusbar).
    fn fast_two_lines(&mut self) {
        let rows = self.screen.list_rows();
        let theme = self.cfg.theme.clone();
        let (w, detail) = (self.pane_width(), self.detail);
        let mut old = None;
        let mut new = None;
        if self.last_cursor != self.dir.cursor {
            if self.last_cursor >= self.dir.scroll
                && self.last_cursor < self.dir.scroll + rows
                && self.last_cursor < self.dir.nvisible()
            {
                old = Some((self.last_cursor, entry_line(
                    &self.dir, self.last_cursor, w, detail, &theme,
                    &self.sel, self.cfg.ui.nerd_font,
                )));
            }
            if self.dir.cursor >= self.dir.scroll
                && self.dir.cursor < self.dir.scroll + rows
                && self.dir.cursor < self.dir.nvisible()
            {
                new = Some((self.dir.cursor, entry_line(
                    &self.dir, self.dir.cursor, w, detail, &theme,
                    &self.sel, self.cfg.ui.nerd_font,
                )));
            }
        }
        let cursor_style = theme.cursor;
        for (idx, line) in [old, new].into_iter().flatten() {
            let row = (2 + idx - self.dir.scroll) as u16;
            let pad = if idx == self.dir.cursor { cursor_style } else { Style::default() };
            self.screen.put_line(row, &line, w, pad);
        }
        self.statusbar();
        self.last_cursor = self.dir.cursor;
    }

    /// Playable width of the entry pane (halved when preview is open).
    fn pane_width(&self) -> usize {
        let w = self.screen.w as usize;
        if self.preview_pane().is_some() {
            (w / 2 - 1).max(20)
        } else {
            w.max(20)
        }
    }

    fn render_full(&mut self) {
        let theme = self.cfg.theme.clone();
        let (w, h) = (self.screen.w, self.screen.h);
        if w < 16 {
            self.screen.put_text(0, 0, "too narrow", Style::default(), w as usize);
            return;
        }

        // Row 0: context bar.
        self.ctxbar(&theme);

        // Row 1: path (left pane only when the preview owns the right half).
        let pw = self.pane_width();
        let shown = tilde_path(&self.dir.path, self.home.as_deref());
        let path_line = Line::plain(shown);
        self.screen.put_line(1, &path_line, pw, theme.statusbar);

        // Listing rows.
        let rows = self.screen.list_rows();
        self.dir.ensure_sorted();
        let scroll = self.dir.scroll_adjust(rows, self.cfg.ui.scrolloff);
        self.last_scroll = scroll;
        self.last_cursor = self.dir.cursor;

        let end = (scroll + rows).min(self.dir.nvisible());
        let mut row = 2u16;
        for i in scroll..end {
            let line = entry_line(&self.dir, i, pw, self.detail, &theme, &self.sel, self.cfg.ui.nerd_font);
            let pad = if i == self.dir.cursor { theme.cursor } else { Style::default() };
            self.screen.put_line(row, &line, pw, pad);
            row += 1;
        }
        // Blank the rest: frames overwrite instead of clearing (no flicker),
        // so rows past the end would otherwise keep the previous frame.
        let blank = Line::default();
        for r in row..2 + rows as u16 {
            self.screen.put_line(r, &blank, pw, Style::default());
        }
        if self.dir.nvisible() == 0 {
            let text = if self.dir.is_empty() { "(empty)" } else { "(no matches)" };
            self.screen.put_text(2, 0, text, theme.divider, pw);
        }

        // Preview pane (right half).
        if let Some((pane_w, _)) = self.preview_pane() {
            self.render_preview(pane_w as usize);
        }

        // Bottom line: statusbar or input/confirm widgets.
        match self.mode.clone() {
            Mode::Normal => self.statusbar(),
            Mode::Filter | Mode::Prompt(_) => self.inputbar(&theme),
            Mode::Confirm(k) => {
                let (n, verb) = match k {
                    ConfirmKind::Trash(v) => (v.len(), "Trash"),
                    ConfirmKind::Delete(v) => (v.len(), "PERMANENTLY delete"),
                };
                let line = Line::styled(format!(" {verb} {n} item(s)? y/n "), theme.error);
                self.screen.put_line(h - 1, &line, w as usize, theme.error);
            }
            Mode::Menu(hl) => {
                self.statusbar();
                self.render_menu(hl);
            }
            Mode::Pager { lines, title, scroll } => self.render_pager(&lines, &title, scroll),
        }
    }

    fn ctxbar(&mut self, theme: &Theme) {
        let mut spans = Vec::with_capacity(CTX_MAX + 1);
        spans.push(Span { text: " ".into(), style: Style::default() });
        for i in (0..CTX_MAX).filter(|&i| self.ctx_open(i)) {
            // The active context shows the live dir; others their saved one.
            let path = if i == self.ctx_i { &self.dir.path } else { &self.ctx[i].path };
            let name = tilde_path(path, self.home.as_deref());
            let short = truncate_cols(&name, 28).0;
            let style = if i == self.ctx_i { theme.ctx_active } else { theme.ctx_inactive };
            spans.push(Span { text: format!("{}:{} ", i + 1, short), style });
        }
        let line = Line::from_spans(spans);
        self.screen.put_line(0, &line, self.screen.w as usize, Style::default());
    }

    fn statusbar(&mut self) {
        let theme = self.cfg.theme.clone();
        let (w, h) = (self.screen.w as usize, self.screen.h);
        let n = self.dir.nvisible();
        let cur = self.dir.cursor.min(n.saturating_sub(1));
        let mut left = format!(" {}/{}", if n == 0 { 0 } else { cur + 1 }, n);
        if self.dir.nvisible() != self.dir.len() {
            left.push_str(&format!(" ({} total)", self.dir.len()));
        }
        if !self.sel.is_empty() {
            left.push_str(&format!(" [{}]", self.sel.len()));
        }
        if let Some(anchor) = self.sel.range_anchor {
            left.push_str(&format!(" range from {anchor}"));
        }
        let msg = self.msg.as_ref().map(|(m, _)| m.clone());
        let right = match self.dir.entries.get(cur) {
            Some(e) => {
                let size = if e.is_dir() { String::new() } else { format!(" {}", coolsize(e.size)) };
                format!("{}{size}", octal3(e.mode))
            }
            None => String::new(),
        };
        let line = if let Some(m) = msg {
            Line::styled(format!("{left} | {m}"), theme.statusbar)
        } else {
            Line::styled(format!("{left} {right}"), theme.statusbar)
        };
        self.screen.put_line(h - 1, &line, w, theme.statusbar);
        self.show_op_progress();
    }

    fn show_op_progress(&mut self) {
        let Some(op) = self.op.as_mut() else { return };
        let Some(p) = op.progress() else { return };
        if p.files_total == 0 || p.files_done >= p.files_total {
            return;
        }
        let pct = p.bytes_done * 100 / p.bytes_total.max(1);
        let cur = truncate_cols(&p.current, 30).0;
        let text = format!(" working: {}/{} files, {}% — {} ", p.files_done, p.files_total, pct, cur);
        let style = self.cfg.theme.statusbar;
        let w = self.screen.w as usize;
        let h = self.screen.h;
        let line = Line::styled(text, style);
        self.screen.put_line(h - 1, &line, w, style);
    }

    fn inputbar(&mut self, theme: &Theme) {
        let (w, h) = (self.screen.w as usize, self.screen.h);
        let line = self.input.render(w.saturating_sub(1));
        self.screen.put_line(h - 1, &line, w, theme.statusbar);
    }

    fn render_menu(&mut self, hl: usize) {
        let theme = self.cfg.theme.clone();
        let w = self.screen.w as usize;
        let mut lines = Vec::new();
        for (shown, &i) in self.menu_items.iter().enumerate() {
            let def = &self.cfg.plugins[i];
            let style = if shown == hl { theme.cursor } else { Style::default() };
            let marker = if shown == hl { "▸ " } else { "  " };
            lines.push(Line::styled(format!("{marker}{}", def.name), style));
        }
        let box_h = (lines.len() + 2).min(self.screen.list_rows());
        let top = (2u16).saturating_add(((self.screen.list_rows() - box_h) / 2) as u16);
        // Frame
        self.screen.put_text(top, 1, &"─".repeat(w.saturating_sub(2)), theme.divider, w);
        self.screen
            .put_text(top + box_h as u16 - 1, 1, &"─".repeat(w.saturating_sub(2)), theme.divider, w);
        for (r, line) in lines.iter().enumerate() {
            self.screen.put_line(top + 1 + r as u16, line, w - 2, theme.ctx_active);
        }
    }

    fn render_pager(&mut self, lines: &[Line], title: &str, scroll: usize) {
        let theme = self.cfg.theme.clone();
        let (w, h) = (self.screen.w as usize, self.screen.h);
        let rows = self.screen.list_rows();
        let meta = Line::styled(format!(" {title}"), theme.preview_meta);
        self.screen.put_line(0, &meta, w, theme.preview_meta);
        // Covers every row under the title so the listing can't show through.
        let start = scroll.min(lines.len().saturating_sub(1));
        let mut body = lines[start..].iter();
        let blank = Line::default();
        for r in 1..1 + rows as u16 + 1 {
            self.screen.put_line(r, body.next().unwrap_or(&blank), w, Style::default());
        }
        self.screen.put_line(h - 1, &Line::plain(" q close, j/k scroll ".to_string()), w, theme.preview_meta);
    }

    fn render_preview(&mut self, pane_w: usize) {
        let theme = self.cfg.theme.clone();
        let h = self.screen.h;
        let x = (self.screen.w as usize).saturating_sub(pane_w);
        // Divider.
        for r in 1..h.saturating_sub(1) {
            self.screen.put_text(r, x.saturating_sub(1) as u16, "│", theme.divider, 1);
        }
        // Header: hovered name (or the pinned plugin and its dir) + state.
        let name = match self.preview_pin.and_then(|p| self.cfg.plugins.get(p.plugin)) {
            Some(def) => format!("{}: {}", def.name, tilde_path(&self.dir.path, self.home.as_deref())),
            None => self.dir.hovered_name().unwrap_or("").to_string(),
        };
        let image = self.preview.image().map(|(seq, _)| seq);
        let state = match (&self.preview.state, self.preview.error()) {
            (_, Some(e)) => format!("error: {e}"),
            // A running job with a frame up is playing, not loading.
            (crate::preview::PreviewState::Running { .. }, _) if image.is_none() => "…".into(),
            (crate::preview::PreviewState::Debounce { .. }, _) => "…".into(),
            _ => String::new(),
        };
        let mut meta = Line::from_spans(vec![
            Span { text: format!(" {name} "), style: theme.preview_meta },
            Span { text: state, style: theme.preview_meta },
        ]);
        if let Some((_, truncated)) = self.preview.lines() {
            if truncated {
                meta.spans.push(Span { text: " (truncated)".into(), style: theme.preview_meta });
            }
        }
        self.screen.put_line_at(1, x as u16, &meta, pane_w, theme.preview_meta);

        // Body. Leave it alone under an image that is up: on terminals that
        // attach images to cells, writing those cells erases the image.
        if image.is_some() && image == self.shown.map(|s| s.seq) {
            return;
        }
        let rows = self.screen.list_rows();
        let mut r = 2u16;
        if let Some((lines, _trunc)) = self.preview.lines() {
            // Head of the output, like a pager: a file preview starts at line 1.
            for line in lines {
                // One column of padding after the divider.
                for seg in wrap_line(line, pane_w.saturating_sub(2)) {
                    self.screen.put_line_at(r, x as u16 + 1, &seg, pane_w - 1, Style::default());
                    r += 1;
                    if r as usize >= 2 + rows {
                        break;
                    }
                }
                if r as usize >= 2 + rows {
                    break;
                }
            }
        }
        let blank = Line::default();
        for rr in r..2 + rows as u16 {
            self.screen.put_line_at(rr, x as u16 + 1, &blank, pane_w - 1, Style::default());
        }
    }
}

/// Line-editing keys for text-entry modes; None = not an edit key.
fn input_event(k: KeyEvent) -> Option<InputEv> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    Some(match k.code {
        KeyCode::Char('w') if ctrl => InputEv::WordBack,
        KeyCode::Char('u') if ctrl => InputEv::Clear,
        KeyCode::Char('h') if ctrl => InputEv::Backspace, // some terminals send ^H
        KeyCode::Char(c) if !ctrl && !alt => InputEv::Char(c),
        KeyCode::Backspace => InputEv::Backspace,
        KeyCode::Delete => InputEv::Delete,
        KeyCode::Left => InputEv::Left,
        KeyCode::Right => InputEv::Right,
        KeyCode::Home => InputEv::Home,
        KeyCode::End => InputEv::End,
        _ => return None,
    })
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn octal3(mode: u32) -> String {
    format!("{:03o}", mode & 0o777)
}

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Quote for embedding a path in a shell command line.
pub fn shquote(s: &str) -> String {
    if !s.bytes().any(|b| b"'\"\\ $&;()<>|*?~".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One listing row, with an optional Nerd Font icon before the name.
fn entry_line(
    dir: &Dir,
    i: usize,
    width: usize,
    detail: bool,
    theme: &Theme,
    sel: &Selection,
    nerd_font: bool,
) -> Line {
    let Some(e) = dir.entries.get(i) else { return Line::default() };
    let selected = dir.path_at(i).as_deref().map(|p| sel.contains(p)).unwrap_or(false) || e.is_selected();
    let ind = if selected { "+" } else { " " };
    let ind_style = if selected { theme.selected } else { Style::default() };
    let style = entry_style(e, theme);
    let mut spans = Vec::with_capacity(if nerd_font { 6 } else { 5 });
    if detail {
        spans.push(Span {
            text: format!("{ind}{:03o} ", e.mode & 0o777),
            style: theme.divider,
        });
        spans.push(Span {
            text: format!("{:<11} ", fmt_time(e.mtime, now_secs())),
            style: theme.divider,
        });
        spans.push(Span {
            text: format!("{:>7} ", coolsize(e.size)),
            style: theme.divider,
        });
    } else {
        spans.push(Span { text: format!("{ind} "), style: ind_style });
    }
    if nerd_font {
        spans.push(Span { text: format!("{} ", entry_icon(e)), style });
    }
    let used = spans.iter().map(|s| crate::ui::str_width(&s.text)).sum::<usize>();
    let avail = width.saturating_sub(used + 2);
    let (name, _) = truncate_cols(&crate::ui::sanitize(&e.name), avail);
    spans.push(Span { text: name, style });
    let ind_ch = e.indicator();
    if ind_ch != ' ' {
        spans.push(Span { text: ind_ch.to_string(), style });
    }
    Line::from_spans(spans)
}

fn entry_style(e: &Entry, theme: &Theme) -> Style {
    if e.flags.has(EntryFlags::SYM_ORPHAN) {
        theme.orphan
    } else if e.flags.has(EntryFlags::FILE_MISSING) {
        theme.missing
    } else if e.is_dir() {
        theme.dir
    } else if e.is_symlink() {
        theme.link
    } else if e.flags.has(EntryFlags::EXEC) {
        theme.exec
    } else if e.flags.has(EntryFlags::HARD_LINK) {
        theme.hardlink
    } else {
        Style::default()
    }
}

/// Picks a Nerd Font glyph from the entry type, filename, or extension.
fn entry_icon(e: &Entry) -> char {
    // Nerd Fonts' Font Awesome, Octicons, Seti, and Devicons codepoints.
    // Links keep their own icon, including links to directories.
    if e.is_symlink() {
        return '\u{f481}'; // nf-oct-file_symlink_file
    }
    if e.is_dir() {
        return '\u{f07b}'; // nf-fa-folder
    }
    match &*e.name {
        "Cargo.toml" | "Cargo.lock" => return '\u{e7a8}',
        "go.mod" | "go.sum" => return '\u{e626}',
        "Dockerfile" | "Containerfile" => return '\u{e7b0}',
        "Makefile" | "GNUmakefile" | "CMakeLists.txt" => return '\u{e615}',
        ".gitignore" | ".gitattributes" | ".gitmodules" => return '\u{e702}',
        _ => {}
    }
    // Compare extensions in ASCII without changing the displayed filename.
    let ext = e.ext().to_ascii_lowercase();
    match ext.as_str() {
        "go" => '\u{e626}',
        "py" | "pyw" | "pyi" | "pyc" | "ipynb" => '\u{e73c}',
        "rs" => '\u{e7a8}',
        "js" | "mjs" | "cjs" | "jsx" => '\u{e74e}',
        "ts" | "mts" | "cts" | "tsx" => '\u{e628}',
        "c" | "h" => '\u{e61e}',
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => '\u{e61d}',
        "cs" => '\u{e648}',
        "java" | "class" | "jar" => '\u{e738}',
        "rb" => '\u{e739}',
        "php" => '\u{e73d}',
        "lua" => '\u{e620}',
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" => '\u{f489}',
        "html" | "htm" => '\u{e736}',
        "css" | "scss" | "sass" | "less" => '\u{e749}',
        "json" | "jsonc" => '\u{e60b}',
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" => '\u{e615}',
        "md" | "markdown" => '\u{e73e}',
        "txt" | "log" | "csv" | "tsv" => '\u{f15c}',
        "pdf" => '\u{f1c1}',
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" | "tif" | "tiff" => '\u{f1c5}',
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" => '\u{f1c7}',
        "mp4" | "mkv" | "mov" | "webm" | "avi" | "m4v" => '\u{f1c8}',
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" => '\u{f1c6}',
        "db" | "sqlite" | "sqlite3" | "sql" => '\u{f1c0}',
        _ if e.flags.has(EntryFlags::EXEC) => '\u{f489}',
        _ => '\u{f15b}', // nf-fa-file: unknown, extensionless, or non-UTF-8 names
    }
}

/// Greedy wrap by display width (preview pane). Blank input yields one
/// blank line so empty files still blank the pane.
fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    if width == 0 {
        return vec![Line::default()];
    }
    let mut out = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    for span in &line.spans {
        let mut start = String::new();
        let style = span.style;
        for c in span.text.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if cur_w + cw > width {
                if !start.is_empty() {
                    cur.push(Span { text: std::mem::take(&mut start), style });
                }
                out.push(Line::from_spans(std::mem::take(&mut cur)));
                cur_w = 0;
                if c == ' ' {
                    continue; // swallow wrap-point spaces
                }
            }
            start.push(c);
            cur_w += cw;
        }
        if !start.is_empty() {
            cur.push(Span { text: start, style });
        }
    }
    if cur.is_empty() && out.is_empty() {
        cur.push(Span { text: String::new(), style: Style::default() });
    }
    if !cur.is_empty() {
        out.push(Line::from_spans(cur));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// Apply `[keys]` remaps; returns the keys the user bound explicitly.
fn apply_key_config(
    map: &mut std::collections::HashMap<KeyEvent, Action>,
    keys: &[(String, String)],
) -> std::collections::HashSet<KeyEvent> {
    let mut bound = std::collections::HashSet::new();
    const NAMES: &[(&str, Action)] = &[
        ("back", Action::Back), ("open", Action::Open), ("nav_in", Action::NavIn),
        ("next", Action::Next), ("prev", Action::Prev), ("pgdn", Action::PgDn),
        ("pgup", Action::PgUp), ("home", Action::Home), ("end", Action::End),
        ("first_file", Action::FirstFile), ("youngest", Action::Youngest),
        ("cd_home", Action::CdHome), ("cd_start", Action::CdStart),
        ("cd_last", Action::CdLast), ("cd_root", Action::CdRoot),
        ("quit", Action::Quit), ("quit_err", Action::QuitErr), ("quit_cd", Action::QuitCd),
        ("quit_ctx", Action::QuitCtx), ("toggle_hidden", Action::ToggleHidden),
        ("toggle_detail", Action::ToggleDetail), ("file_info", Action::FileInfo),
        ("cycle_sort", Action::CycleSort), ("reverse_sort", Action::ReverseSort),
        ("cycle_time", Action::CycleTime), ("redraw", Action::Redraw),
        ("toggle_preview", Action::TogglePreview), ("help", Action::Help),
        ("filter", Action::Filter), ("cycle_filter_mode", Action::CycleFilterMode),
        ("select", Action::Select), ("select_mul", Action::SelectMul),
        ("select_all", Action::SelectAll), ("select_invert", Action::SelectInvert),
        ("sel_edit", Action::SelEdit), ("sel_size", Action::SelSize),
        ("copy", Action::Copy), ("move", Action::Move), ("copy_as", Action::CopyAs),
        ("trash", Action::Trash), ("delete", Action::Delete), ("new", Action::New),
        ("rename", Action::Rename), ("open_with", Action::OpenWith), ("edit", Action::Edit),
        ("shell", Action::Shell), ("plugin_menu", Action::PluginMenu),
    ];
    for (name, key) in keys {
        let Some(action) = NAMES.iter().find(|(n, _)| n == name).map(|(_, a)| a.clone()) else {
            continue;
        };
        if let Some(k) = crate::action::parse_key(key) {
            map.retain(|_, v| *v != action); // one key per action: drop old binding
            map.insert(k, action);
            bound.insert(k);
        }
    }
    bound
}

fn help_lines(plugins: &[crate::config::PluginDef]) -> Vec<Line> {
    const H: &[&str] = &[
        "vfm — keys",
        "",
        "  j/k or arrows      move            h/Left       parent dir",
        "  l/Right            enter dir       Enter        open (dir/media/text)",
        "  g / G              top / bottom    C-d / C-u    half page",
        "  J ±n               jump n rows     C-y          newest entry",
        "  '                  first file",
        "",
        "  ~ / @ / - / `      home / start / last / root dir",
        "  Tab / S-Tab        cycle contexts  1-8          jump to context",
        "  q                  close context   C-q quit     Q quit(1)",
        "  C-g                quit + write cd path ($VFM_CD_FILE)",
        "",
        "  /                  filter (Enter keeps, Esc clears, C-n mode)",
        "  s / C-t            sort key / reverse      T  time type",
        "  .                  show hidden     d            detail toggle",
        "  f                  file info       P            preview toggle",
        "  ?                  this help",
        "",
        "  Space              select + move   m m          range select",
        "  a / A              select all / invert    E     edit selection",
        "  S                  selection size",
        "  p / v              paste copy / move here     w  copy as…",
        "  x / X              trash / delete (confirmed)",
        "  n / C-r            new / rename    o            open with…",
        "  e                  edit in $EDITOR  !           shell in cwd",
        "  ;                  plugin menu",
        "",
        "  Esc                cancel / unpin / clear filter / close overlays",
    ];
    let mut lines: Vec<Line> = H.iter().map(|s| Line::plain((*s).to_string())).collect();
    if !plugins.is_empty() {
        lines.push(Line::default());
        lines.push(Line::plain("Plugins (; for the menu)"));
        for p in plugins {
            let key = p.key.as_deref().filter(|k| !k.is_empty()).unwrap_or("-");
            let mut row = format!("  {key:<18} {}", p.name);
            if let Some(prog) = crate::plugin::missing_requirement(&p.requires) {
                row.push_str(&format!("  (needs `{prog}`, not installed)"));
            }
            lines.push(Line::plain(row));
        }
    }
    lines
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // Scripted event source: a plain queue, no channel (try_recv would eat
    // events during poll probing).
    struct TestSource {
        q: std::sync::Mutex<Vec<std::collections::VecDeque<Event>>>,
    }
    impl TestSource {
        fn new() -> TestSource {
            TestSource { q: std::sync::Mutex::new(vec![Default::default()]) }
        }
        fn push(&self, e: Event) {
            self.q.lock().unwrap()[0].push_back(e);
        }
    }
    impl EventSource for TestSource {
        fn poll(&mut self, _t: Duration) -> io::Result<bool> {
            Ok(true)
        }
        /// Once the script runs dry it sends C-q, so `run()` always ends.
        fn read(&mut self) -> io::Result<Event> {
            Ok(self.q.lock().unwrap()[0].pop_front().unwrap_or_else(|| ckey('q')))
        }
    }

    /// Captures what Screen flushes (Screen::frame() only sees the unflushed
    /// buffer, and render() flushes).
    #[derive(Clone, Default)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl io::Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl SharedBuf {
        fn take(&self) -> String {
            String::from_utf8_lossy(&std::mem::take(&mut *self.0.borrow_mut())).into_owned()
        }
    }

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }
    fn ckey(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_app(name: &str) -> (App, PathBuf) {
        let d = tmpdir(name);
        for n in ["alpha", "beta", "gamma"] {
            std::fs::write(d.join(n), b"").unwrap();
        }
        std::fs::create_dir_all(d.join("subdir")).unwrap();
        let src = TestSource::new();
        let screen = Screen::new(Box::new(Vec::new()), 120, 30);
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(src)).unwrap();
        // Never touch the real ~/.config/vfm/selection from tests.
        app.sel.file = std::env::temp_dir().join(format!("vfm-sel-{name}-{}", std::process::id()));
        app.preview.highlight = None; // the same output whether or not bat is installed
        (app, d)
    }

    fn special(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Full event path: crossterm events → key_action routing → run loop.
    #[test]
    fn scripted_session_through_run_loop() {
        let d = tmpdir("script");
        std::fs::write(d.join("keep.txt"), b"k").unwrap();
        let src = TestSource::new();
        // n → New prompt; the typed text must NOT trigger bound actions
        // (x = trash, d = detail, . = hidden, s = sort, t = tree).
        src.push(key('n'));
        for c in "xd.st".chars() {
            src.push(key(c));
        }
        src.push(special(KeyCode::Backspace));
        src.push(special(KeyCode::Enter)); // creates "xd.s"
        // / filter: 'k' matches keep.txt only; Enter keeps it.
        src.push(key('/'));
        src.push(key('k'));
        src.push(special(KeyCode::Enter));
        // x → trash confirm; 'n' cancels (anything but y).
        src.push(key('x'));
        src.push(key('n'));
        let screen = Screen::new(Box::new(io::sink()), 100, 30);
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(src)).unwrap();
        app.sel.file = d.join(".sel");
        let flow = app.run();
        assert_eq!(flow, Flow::Quit(0)); // trailing C-q
        assert!(d.join("xd.s").is_file(), "prompt text was hijacked by keybindings");
        assert!(app.detail, "'d' typed into the prompt toggled detail mode");
        assert_eq!(app.ctx[0].filter_text.as_deref(), Some("k"));
        assert_eq!(app.dir.nvisible(), 1);
        assert!(d.join("keep.txt").exists(), "trash ran without confirmation");
        assert!(matches!(app.mode, Mode::Normal));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The probe owns tty input: the run loop reads no events until its
    /// report lands, then replays the swallowed keys before live input.
    #[test]
    fn terminal_probe_gates_input_and_replays_typeahead() {
        let d = tmpdir("probe");
        for n in ["alpha", "beta", "gamma"] {
            std::fs::write(d.join(n), b"").unwrap();
        }
        let src = TestSource::new();
        // Replayed G (bottom: gamma) then live k (up: beta). Live k first
        // would wrap alpha → gamma and G would stay there.
        src.push(key('k'));
        let screen = Screen::new(Box::new(io::sink()), 100, 30);
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(src)).unwrap();
        app.sel.file = d.join(".sel");
        let (tx, probe) = term::Probe::channel();
        app.probe = Some(probe);
        // Report late, so a loop ignoring the gate reads k and the trailing
        // C-q before the report exists.
        let reporter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let typeahead = vec![KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)];
            tx.send(term::Report { graphics: true, typeahead }).unwrap();
        });
        assert_eq!(app.run(), Flow::Quit(0)); // trailing C-q
        reporter.join().unwrap();
        assert_eq!(app.term, term::Kind::KittyGraphics);
        assert!(app.graphics && app.probe.is_none());
        assert_eq!(app.dir.hovered_name(), Some("beta"));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn act(app: &mut App, a: Action) -> Option<Flow> {
        app.apply(a)
    }

    #[test]
    fn navigation_and_two_lines() {
        let (mut app, d) = test_app("nav");
        // dirs_first: subdir sorts before the files
        assert_eq!(app.dir.hovered_name(), Some("subdir"));
        act(&mut app, Action::Next);
        assert_eq!(app.dir.hovered_name(), Some("alpha"));
        act(&mut app, Action::Prev);
        act(&mut app, Action::Prev); // wraps past the top to the last entry
        assert_eq!(app.dir.hovered_name(), Some("gamma"));
        act(&mut app, Action::End);
        assert_eq!(app.dir.hovered_name(), Some("gamma"));
        act(&mut app, Action::Home);
        assert_eq!(app.dir.hovered_name(), Some("subdir"));
        act(&mut app, Action::NavIn);
        assert!(app.dir.path.ends_with("subdir"));
        act(&mut app, Action::Back);
        assert_eq!(app.dir.path, d);
        assert_eq!(app.dir.hovered_name(), Some("subdir")); // cursor restored
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn left_at_root_keeps_current_context_open() {
        let (mut app, d) = test_app("root-back");
        act(&mut app, Action::CdRoot);
        for context in [0, 1] {
            act(&mut app, Action::GotoCtx(context));
            let context = usize::from(context);
            let active = app.ctx_active;
            let cursor = app.dir.cursor;
            let last_dir = app.ctx[context].last_dir.clone();
            for _ in 0..3 {
                assert_eq!(app.handle_event(special(KeyCode::Left)), None);
                assert_eq!(app.dir.path, Path::new("/"));
                assert_eq!(app.dir.cursor, cursor);
                assert_eq!((app.ctx_i, app.ctx_active), (context, active));
                assert_eq!(app.ctx[context].last_dir, last_dir);
                assert_eq!(app.dirty, Dirty::None);
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn filter_live_and_accept() {
        let (mut app, d) = test_app("filter");
        act(&mut app, Action::Filter);
        assert!(matches!(app.mode, Mode::Filter));
        act(&mut app, Action::Input(InputEv::Char('a')));
        act(&mut app, Action::Input(InputEv::Char('l')));
        // "al" matches only alpha
        assert_eq!(app.dir.nvisible(), 1);
        assert_eq!(app.dir.hovered_name(), Some("alpha"));
        // Esc clears
        act(&mut app, Action::Esc);
        assert_eq!(app.dir.nvisible(), 4);
        assert!(matches!(app.mode, Mode::Normal));
        // Enter keeps the filter
        act(&mut app, Action::Filter);
        act(&mut app, Action::Input(InputEv::Char('g')));
        act(&mut app, Action::Open);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.dir.nvisible(), 1);
        assert_eq!(app.dir.hovered_name(), Some("gamma"));
        assert_eq!(app.ctx[0].filter_text.as_deref(), Some("g"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn contexts_switch_and_isolate() {
        let (mut app, d) = test_app("ctx");
        act(&mut app, Action::NavIn); // ctx 1 → subdir
        act(&mut app, Action::GotoCtx(1));
        assert_eq!((app.ctx_i, app.ctx_active), (1, 0b11));
        assert_eq!(app.dir.path, d.join("subdir")); // fresh ctx inherits cwd
        act(&mut app, Action::Back);
        act(&mut app, Action::ToggleHidden);
        assert!(app.ctx[1].show_hidden && !app.ctx[0].show_hidden);
        // Jumping to 8 opens only slot 8, not 3..7.
        act(&mut app, Action::GotoCtx(7));
        assert_eq!(app.ctx_active, 0b1000_0011);
        // Tab skips closed slots and wraps: 8 → 1 → 2 → 8.
        act(&mut app, Action::CycleCtx(1));
        assert_eq!(app.ctx_i, 0);
        assert_eq!(app.dir.path, d.join("subdir")); // ctx 1 kept its dir
        act(&mut app, Action::CycleCtx(1));
        assert_eq!(app.ctx_i, 1);
        assert_eq!(app.dir.path, d);
        act(&mut app, Action::CycleCtx(-1));
        assert_eq!(app.ctx_i, 0);
        // Closing the middle-index slot leaves the others usable.
        act(&mut app, Action::GotoCtx(1));
        assert_eq!(act(&mut app, Action::QuitCtx), Some(Flow::Continue));
        assert_eq!((app.ctx_i, app.ctx_active), (7, 0b1000_0001));
        assert_eq!(act(&mut app, Action::QuitCtx), Some(Flow::Continue));
        assert_eq!((app.ctx_i, app.ctx_active), (0, 0b1));
        assert_eq!(act(&mut app, Action::QuitCtx), Some(Flow::Quit(0)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn selection_flags_and_range() {
        let (mut app, d) = test_app("sel");
        act(&mut app, Action::Select); // selects subdir (hovered), advances
        assert!(app.sel.contains(&d.join("subdir")));
        assert_eq!(app.dir.hovered_name(), Some("alpha"));
        act(&mut app, Action::SelectMul); // range anchor at alpha
        act(&mut app, Action::Next); // beta
        act(&mut app, Action::SelectMul); // completes range alpha..beta
        assert!(app.sel.contains(&d.join("alpha")));
        assert!(app.sel.contains(&d.join("beta")));
        assert_eq!(app.sel.len(), 3);
        // NUL-separated disk mirror (roundtrip itself is covered in selection.rs)
        let f = app.sel.file.clone();
        let data = std::fs::read(&f).unwrap();
        assert_eq!(data.iter().filter(|&&b| b == 0).count(), 3);
        // Invert over the 4-entry listing leaves only gamma selected.
        act(&mut app, Action::SelectInvert);
        assert_eq!(app.sel.items(), &[d.join("gamma")]);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(f);
    }

    #[test]
    fn quit_cd_and_flows() {
        let (mut app, d) = test_app("quit");
        assert_eq!(act(&mut app, Action::Quit), Some(Flow::Quit(0)));
        let (mut app, d2) = test_app("quit2");
        assert_eq!(act(&mut app, Action::QuitErr), Some(Flow::Quit(1)));
        let (mut app, d3) = test_app("quit3");
        assert_eq!(act(&mut app, Action::QuitCd), Some(Flow::Quit(0)));
        assert_eq!(app.quit_cd.as_deref(), Some(d3.as_path()));
        for n in [d, d2, d3] {
            let _ = std::fs::remove_dir_all(n);
        }
    }

    #[test]
    fn render_snapshot_light_and_detail() {
        let (mut app, d) = test_app("render");
        let out = SharedBuf::default();
        app.screen = Screen::new(Box::new(out.clone()), 120, 30);
        app.render();
        let frame = out.take();
        assert!(frame.contains("alpha"), "frame: {frame}");
        assert!(frame.contains(&*d.to_string_lossy()));
        // Cursor row is reversed (SGR 7) and bold (1).
        assert!(frame.contains("\x1b[0;1;7m"), "cursor style missing: {frame}");
        // Detail view (the default) shows octal perms + size; light mode doesn't.
        let alpha = app.dir.entries.iter().find(|e| &*e.name == "alpha").unwrap();
        let perms = format!(" {:03o} ", alpha.mode & 0o777);
        assert!(app.detail);
        assert!(frame.contains(&perms), "perms {perms:?} missing: {frame}");
        act(&mut app, Action::ToggleDetail);
        app.render();
        let frame = out.take();
        assert!(frame.contains("alpha") && !frame.contains(&perms), "light mode: {frame}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nerd_font_icons_follow_file_types_and_keep_directory_and_link_identity() {
        let mut entry = Entry { name: "main.go".into(), size: 0, mtime: 0, mode: 0, flags: EntryFlags::default() };
        for (name, icon) in [
            ("main.go", '\u{e626}'),
            ("main.GO", '\u{e626}'),
            ("script.py", '\u{e73c}'),
            ("script.PY", '\u{e73c}'),
            ("types.pyi", '\u{e73c}'),
            ("go.mod", '\u{e626}'),
            ("Cargo.toml", '\u{e7a8}'),
            ("Dockerfile", '\u{e7b0}'),
            ("unknown.extension", '\u{f15b}'),
            ("no-extension", '\u{f15b}'),
            ("non-utf8-\u{fffd}", '\u{f15b}'),
        ] {
            entry.name = name.into();
            assert_eq!(entry_icon(&entry), icon, "{name}");
        }
        entry.flags.set(EntryFlags::EXEC);
        assert_eq!(entry_icon(&entry), '\u{f489}');
        entry.name = "executable.py".into();
        assert_eq!(entry_icon(&entry), '\u{e73c}');
        entry.flags.set(EntryFlags::DIR_OR_DIRLNK);
        assert_eq!(entry_icon(&entry), '\u{f07b}');
        entry.flags.set(EntryFlags::SYMLINK);
        assert_eq!(entry_icon(&entry), '\u{f481}');
        entry.flags.set(EntryFlags::SYM_ORPHAN);
        assert_eq!(entry_icon(&entry), '\u{f481}');
    }

    #[test]
    fn nerd_font_icons_render_in_both_views_and_cursor_redraws() {
        let (mut app, d) = test_app("nerd-font-render");
        for name in ["main.go", "script.PY"] {
            std::fs::write(d.join(name), b"").unwrap();
        }
        app.reload_dir();
        let out = SharedBuf::default();
        app.screen = Screen::new(Box::new(out.clone()), 120, 30);
        for detail in [true, false] {
            app.detail = detail;
            app.cfg.ui.nerd_font = true;
            app.dir.cursor = 0;
            app.dirty = Dirty::Full;
            app.render();
            let frame = out.take();
            for text in ["\u{e626} main.go", "\u{e73c} script.PY", "\u{f07b} subdir/", "\u{f15b} alpha"] {
                assert!(frame.contains(text), "detail={detail}: {frame:?}");
            }
            // Navigation redraws only the old and new cursor rows.
            app.dir.move_to_name("main.go");
            app.dirty = Dirty::TwoLines;
            app.render();
            let frame = out.take();
            assert!(frame.contains("\u{e626} main.go"), "{frame:?}");
            assert!(!frame.contains("\x1b[1;1H"), "{frame:?}");
            act(&mut app, Action::Next);
            app.render();
            let frame = out.take();
            assert!(frame.contains("\u{e626} main.go") && frame.contains("\u{e73c} script.PY"), "{frame:?}");

            app.cfg.ui.nerd_font = false;
            app.dirty = Dirty::Full;
            app.render();
            let frame = out.take();
            assert!(frame.contains("main.go") && frame.contains("script.PY"));
            assert!(!frame.contains(['\u{e626}', '\u{e73c}', '\u{f07b}', '\u{f15b}']), "{frame:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nerd_font_rows_preserve_selection_styles_and_truncate_by_display_width() {
        let (mut app, d) = test_app("nerd-font-width");
        let i = app.dir.entries.iter().position(|e| &*e.name == "alpha").unwrap();
        app.dir.entries[i].name = "wide-你好-long-filename.py".into();
        app.dir.entries[i].flags.set(EntryFlags::EXEC);
        // Select through the path set, without relying on the cached entry flag.
        app.sel.add(&app.dir.path_at(i).unwrap());
        for detail in [true, false] {
            for width in [32, 40, 60] {
                let line = entry_line(&app.dir, i, width, detail, &app.cfg.theme, &app.sel, true);
                let text = line.text();
                assert!(text.starts_with('+') && text.ends_with('*'), "{text:?}");
                assert!(text.contains("\u{e73c} "), "{text:?}");
                assert!(crate::ui::str_width(&text) < width, "{width}: {text:?}");
                let icon = line.spans.iter().find(|s| s.text == "\u{e73c} ").unwrap();
                assert_eq!(icon.style, app.cfg.theme.exec);
                assert_eq!(line.spans.last().unwrap().style, app.cfg.theme.exec);
                if !detail {
                    assert_eq!(line.spans[0].style, app.cfg.theme.selected);
                }
                if detail && width == 32 {
                    assert!(text.contains('…'), "{text:?}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_lines_fast_path_emits_little() {
        let (mut app, d) = test_app("twolines");
        let out = SharedBuf::default();
        app.screen = Screen::new(Box::new(out.clone()), 120, 30);
        app.render();
        let full_len = out.take().len();
        act(&mut app, Action::Next);
        assert_eq!(app.dirty, Dirty::TwoLines);
        app.render();
        let fast = out.take();
        assert!(!fast.is_empty());
        assert!(fast.len() < 512, "fast path emitted {} bytes (full was {full_len})", fast.len());
        // Only the two entry rows + statusbar: the header rows are untouched.
        assert!(!fast.contains("\x1b[1;1H") && !fast.contains("\x1b[2;1H"), "{fast:?}");
        assert!(fast.contains("\x1b[3;1H") && fast.contains("\x1b[4;1H"), "{fast:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fast_path_keeps_cursor_in_scrolloff_band() {
        let d = tmpdir("band");
        for i in 0..50 {
            std::fs::write(d.join(format!("f{i:02}")), b"").unwrap();
        }
        let out = SharedBuf::default();
        let screen = Screen::new(Box::new(out.clone()), 80, 13); // 10 list rows
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(TestSource::new())).unwrap();
        app.render();
        let rows = app.screen.list_rows();
        for step in 1..=30 {
            act(&mut app, Action::Next);
            app.render();
            let (cur, scroll) = (app.dir.cursor, app.dir.scroll);
            assert_eq!(cur, step);
            // Cursor stays on screen and >= SCROLLOFF rows from the bottom edge.
            assert!(cur >= scroll && cur + crate::dir::SCROLLOFF < scroll + rows, "step {step}: cur {cur} scroll {scroll}");
            // And the screen really shows it: the hovered name was emitted.
            assert!(out.take().contains(&format!("f{cur:02}")), "step {step}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn filter_20k_entries_under_8ms() {
        let d = tmpdir("perf");
        for i in 0..20_000 {
            std::fs::write(d.join(format!("entry-{i:05}-file.rs")), b"").unwrap();
        }
        let src = TestSource::new();
        let screen = Screen::new(Box::new(Vec::new()), 120, 30);
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(src)).unwrap();
        // Warm sort.
        app.render();
        act(&mut app, Action::Filter);
        let t0 = Instant::now();
        for c in "ent".chars() {
            act(&mut app, Action::Input(InputEv::Char(c)));
        }
        // Plan target: <8ms per keystroke (release). Debug builds get slack.
        let budget = Duration::from_millis(if cfg!(debug_assertions) { 60 } else { 8 });
        let dt = t0.elapsed() / 3;
        assert_eq!(app.dir.nvisible(), 20_000); // "ent" matches all
        assert!(dt < budget, "{dt:?}/keystroke (all match)");
        let t0 = Instant::now();
        for c in "ry-19999".chars() {
            act(&mut app, Action::Input(InputEv::Char(c)));
        }
        let dt = t0.elapsed() / 8;
        assert_eq!(app.dir.nvisible(), 1);
        assert!(dt < budget, "{dt:?}/keystroke (narrowing)");
        // Backspacing all the way out restores the full, sorted listing.
        for _ in 0..11 {
            act(&mut app, Action::Input(InputEv::Backspace));
        }
        assert_eq!(app.dir.nvisible(), 20_000);
        assert!(app.dir.entries.windows(2).all(|w| crate::sort::natural_cmp(&w[0].name, &w[1].name).is_le()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn input_line_editing() {
        let mut i = InputLine::new("> ");
        for c in "hello world".chars() {
            i.insert(c);
        }
        i.word_back();
        i.clear();
        i.insert('x');
        i.backspace();
        i.backspace(); // no-op at empty
        assert_eq!(i.value, "");
        // Multibyte: backspace removes whole chars, never splits UTF-8.
        let mut i = InputLine::default();
        for c in "日本語".chars() {
            i.insert(c);
        }
        i.backspace();
        assert_eq!(i.value, "日本");
        i.left();
        i.backspace();
        assert_eq!(i.value, "本");
        let mut i = InputLine::default();
        for c in "abc".chars() {
            i.insert(c);
        }
        i.left();
        i.left();
        i.delete(); // deletes 'b', cursor stays between a|c
        assert_eq!(i.value, "ac");
        i.insert('X'); // aXc
        assert_eq!(i.value, "aXc");
        i.home();
        i.right();
        i.insert('Q');
        assert_eq!(i.value, "aQXc");
    }

    #[test]
    fn wrap_line_ansi_and_width() {
        let l = Line::from_spans(vec![
            Span { text: "hello ".into(), style: Style::default() },
            Span { text: "wide世界".into(), style: Style::default() },
        ]);
        // Column wrap (like nnn's preview), styles kept across the split.
        let joined: Vec<String> = wrap_line(&l, 8).iter().map(|l| l.text()).collect();
        assert_eq!(joined, vec!["hello wi", "de世界"]);
        // A wide char never straddles the edge: 世 (2 cols) moves down.
        let joined: Vec<String> = wrap_line(&l, 11).iter().map(|l| l.text()).collect();
        assert_eq!(joined, vec!["hello wide", "世界"]);
        let empty = wrap_line(&Line::plain(""), 10);
        assert_eq!(empty.len(), 1);
    }

    fn plugin(name: &str, cmd: &str, requires: &[&str]) -> crate::config::PluginDef {
        crate::config::PluginDef {
            name: name.into(),
            cmd: cmd.into(),
            key: None,
            requires: requires.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[cfg(unix)]
    fn drive_preview(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.preview.is_busy() && Instant::now() < deadline {
            app.preview.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    fn preview_texts(app: &App) -> Vec<String> {
        app.preview.lines().map(|(l, _)| l.iter().map(|l| l.text()).collect()).unwrap_or_default()
    }

    /// An app on `pic.png` + `notes.txt`, with a graphics previewer for
    /// `*.png` whose frame's image id is the pane size it was given ("WH").
    #[cfg(unix)]
    fn graphics_app(name: &str) -> (App, PathBuf, SharedBuf) {
        let d = tmpdir(name);
        std::fs::write(d.join("pic.png"), b"png").unwrap();
        std::fs::write(d.join("notes.txt"), b"text\n").unwrap();
        let previewers = vec![crate::config::PreviewerRule {
            patterns: vec!["*.png".into()],
            cmd: r#"sh -c "printf '\033_Ga=T,i=%s%s;AAAA\033\\\\' \"\$2\" \"\$3\"" sh"#.into(),
            graphics: true,
        }];
        let cfg = Config { previewers, ..Config::default() };
        let out = SharedBuf::default();
        let screen = Screen::new(Box::new(out.clone()), 120, 30);
        let mut app = App::new(cfg, d.clone(), screen, Box::new(TestSource::new())).unwrap();
        app.sel.file = d.join(".sel");
        app.preview.highlight = None;
        app.preview.debounce = Duration::ZERO;
        app.preview.enabled = true;
        app.graphics = true;
        app.dir.move_to_name("pic.png");
        (app, d, out)
    }

    /// The pane body's top-left cell on a 120x30 screen, as a CUP sequence.
    #[cfg(unix)]
    const BODY_ORIGIN: &str = "\x1b[3;63H";

    #[cfg(unix)]
    #[test]
    fn graphics_previews_need_the_probe_and_fill_the_pane_body() {
        let (mut app, d, _) = graphics_app("gfxprobe");
        app.graphics = false;
        app.preview_on_hover();
        drive_preview(&mut app);
        assert!(app.preview.image().is_none(), "graphics rule ran without graphics");
        assert!(preview_texts(&app)[0].starts_with("png"), "{:?}", preview_texts(&app));

        // The probe's report re-picks the previewer for the hovered file.
        let report = term::Report { graphics: true, typeahead: Vec::new() };
        assert_eq!(app.settle_term(report), None);
        drive_preview(&mut app);
        // 120 cols: a 59-col pane minus its padding column; 30 rows minus 3.
        assert_eq!(app.preview.image().map(|(_, f)| f.id), Some(Some(5827)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn preview_image_follows_hover_overlays_and_quit() {
        let (mut app, d, out) = graphics_app("gfxshow");
        app.preview_on_hover();
        drive_preview(&mut app);
        app.dirty = Dirty::Full;
        app.render();
        // The body is blanked first, then the image goes over it.
        let frame = out.take();
        let at = frame.rfind(BODY_ORIGIN).expect("cursor to the pane body");
        assert!(frame[at..].starts_with("\x1b[3;63H\x1b_Ga=T,i=5827,q=2,C=1;AAAA\x1b\\"), "{:?}", &frame[at..]);
        assert!(frame[..at].contains("\x1b[29;63H"), "body rows blanked under the image");

        // Redraws neither resend the image nor write the body under it.
        app.dirty = Dirty::Full;
        app.render();
        let redraw = out.take();
        assert!(!redraw.contains(BODY_ORIGIN) && !redraw.contains("\x1b_G"), "{redraw:?}");

        // Overlays take the image down; closing them puts it back.
        act(&mut app, Action::PluginMenu);
        app.render();
        assert!(out.take().contains("\x1b_Ga=d,d=I,i=5827,q=2\x1b\\"));
        act(&mut app, Action::Esc);
        app.render();
        assert!(out.take().contains("\x1b_Ga=T,i=5827"));

        // Hovering a text file deletes the image before its preview is ready.
        act(&mut app, Action::Next);
        assert_eq!(app.dir.hovered_name(), Some("notes.txt"));
        app.render();
        assert!(out.take().contains("\x1b_Ga=d,d=I,i=5827,q=2\x1b\\"));

        // Quitting with an image up deletes it.
        act(&mut app, Action::Prev);
        drive_preview(&mut app);
        app.render();
        assert!(out.take().contains("\x1b_Ga=T,i=5827"));
        assert_eq!(app.run(), Flow::Quit(0)); // TestSource sends C-q
        assert!(out.take().ends_with("\x1b_Ga=d,d=I,i=5827,q=2\x1b\\"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn stepping_aside_stops_the_previewer_and_restarts_it_after() {
        let (mut app, d, _) = graphics_app("gfxsuspend");
        // Keeps running after its frame, as a playing video does.
        app.cfg.previewers[0].cmd = r#"sh -c "printf '\033_Ga=T,i=1;AAAA\033\\\\'; sleep 30""#.into();
        app.preview_on_hover();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.preview.image().is_none() && Instant::now() < deadline {
            app.preview.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(app.preview.state, crate::preview::PreviewState::Running { .. }));
        app.suspend_and_run("true", vec![]);
        // The running job was dropped (killed); a fresh one is on its way.
        assert!(matches!(app.preview.state, crate::preview::PreviewState::Debounce { .. }));
        assert!(app.preview.image().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn new_frames_go_up_before_the_old_one_is_deleted() {
        let (mut app, d, out) = graphics_app("gfxswap");
        app.cfg.previewers[0].cmd =
            r#"sh -c "printf '\033_Ga=T,i=1;AAAA\033\\\\'; sleep 0.2; printf '\033_Ga=T,i=2;AAAA\033\\\\'""#.into();
        app.preview_on_hover();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = String::new();
        while app.preview.is_busy() && Instant::now() < deadline {
            app.on_tick();
            app.render();
            output.push_str(&out.take());
            std::thread::sleep(Duration::from_millis(2));
        }
        let second = output.find("\x1b_Ga=T,i=2").expect("second frame");
        let delete = output.find("\x1b_Ga=d,d=I,i=1,q=2").expect("first frame deleted");
        assert!(output.find("\x1b_Ga=T,i=1").unwrap() < second && second < delete, "{output:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn preview_plugin_pins_pane_until_navigation() {
        let (mut app, d) = test_app("pin"); // 120 cols: the pane fits
        app.cfg.plugins = vec![plugin("lister", "> printf 'IN %s\\n' \"$(basename \"$PWD\")\"; ls", &["sh"])];
        app.preview.debounce = Duration::ZERO;
        let base = d.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!app.preview.enabled);

        act(&mut app, Action::PluginRun(0));
        assert!(app.preview.enabled && app.preview_pin.is_some());
        drive_preview(&mut app);
        assert_eq!(preview_texts(&app), [format!("IN {base}"), "alpha".into(), "beta".into(), "gamma".into(), "subdir".into()]);
        // The pane header names the plugin, not the hovered file.
        let out = SharedBuf::default();
        app.screen = Screen::new(Box::new(out.clone()), 120, 30);
        app.dirty = Dirty::Full;
        app.render();
        assert!(out.take().contains("lister: "));

        // A directory refresh reruns it (output stays current).
        std::fs::write(d.join("delta"), b"").unwrap();
        app.reload_dir();
        drive_preview(&mut app);
        assert!(preview_texts(&app).contains(&"delta".to_string()));
        assert!(app.preview_pin.is_some());

        // Navigating unpins and restores the closed pane.
        act(&mut app, Action::Next);
        assert!(app.preview_pin.is_none() && !app.preview.enabled);

        // Same key twice toggles; Esc unpins too.
        act(&mut app, Action::PluginRun(0));
        act(&mut app, Action::PluginRun(0));
        assert!(app.preview_pin.is_none() && !app.preview.enabled);
        act(&mut app, Action::PluginRun(0));
        act(&mut app, Action::Esc);
        assert!(app.preview_pin.is_none() && !app.preview.enabled);

        // With the hover preview already on, unpinning returns to it.
        act(&mut app, Action::TogglePreview);
        act(&mut app, Action::PluginRun(0));
        act(&mut app, Action::Prev);
        assert!(app.preview_pin.is_none() && app.preview.enabled);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn preview_plugin_falls_back_to_pager_when_narrow() {
        let (mut app, d) = test_app("pinnarrow");
        app.screen = Screen::new(Box::new(io::sink()), 80, 24);
        app.cfg.plugins = vec![plugin("colors", "> printf '\\033[31mred\\033[0m\\033[2J ok\\n'", &[])];
        act(&mut app, Action::PluginRun(0));
        assert!(app.preview_pin.is_none());
        let Mode::Pager { lines, title, .. } = &app.mode else { panic!("expected pager") };
        assert_eq!(title, "colors");
        // ANSI parsed: SGR kept as a styled span, the screen-clear dropped.
        assert_eq!(lines[0].text(), "red ok");
        assert_ne!(lines[0].spans[0].style, Style::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plugin_with_missing_program_refuses_to_run() {
        let (mut app, d) = test_app("pinmissing");
        app.cfg.plugins = vec![plugin("ghost", "> ghost-tool", &["vfm-no-such-program-xyz"])];
        act(&mut app, Action::PluginRun(0));
        assert!(app.preview_pin.is_none() && !app.preview.enabled);
        let msg = &app.msg.as_ref().unwrap().0;
        assert!(msg.contains("needs `vfm-no-such-program-xyz`"), "{msg}");
        // Help marks it as unavailable.
        let help: Vec<String> = help_lines(&app.cfg.plugins).iter().map(|l| l.text()).collect();
        assert!(help.iter().any(|l| l.contains("ghost") && l.contains("not installed")), "{help:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tree_plugin_bound_to_t_and_sort_to_s_unless_keys_claim_t() {
        let t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        let s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        let src = || Box::new(TestSource::new());
        let d = tmpdir("treekey");
        let mut app = App::new(Config::default(), d.clone(), Screen::new(Box::new(io::sink()), 80, 24), src()).unwrap();
        let i = app.cfg.plugins.iter().position(|p| p.name == "tree").unwrap();
        assert_eq!(app.keymap[&t], Action::PluginRun(i));
        assert_eq!(app.keymap[&s], Action::CycleSort);
        let next_sort = app.ctx[app.ctx_i].sort.next();
        assert_eq!(app.handle_event(key('s')), None);
        assert_eq!(app.ctx[app.ctx_i].sort, next_sort);
        assert_eq!(app.dir.sort, next_sort);

        let cfg = Config { keys: vec![("select".into(), "t".into())], ..Config::default() };
        let app = App::new(cfg, d.clone(), Screen::new(Box::new(io::sink()), 80, 24), src()).unwrap();
        assert_eq!(app.keymap[&t], Action::Select);
        assert_eq!(app.keymap[&s], Action::CycleSort);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The shipped plugin against the real `tree`, when it's installed.
    #[cfg(unix)]
    #[test]
    fn builtin_tree_plugin_shows_highlighted_folder_in_pane_and_pager() {
        if !crate::plugin::which("tree") {
            eprintln!("skipping: tree not installed");
            return;
        }
        let (mut app, d) = test_app("treereal");
        let folder = d.join("selected '$ folder");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        std::fs::write(folder.join("nested/inner.txt"), b"").unwrap();
        std::fs::write(folder.join("inside.txt"), b"").unwrap();
        app.reload_dir();
        app.dir.move_to_name("selected '$ folder");
        let folder = app.dir.hovered_path().unwrap();
        app.preview.debounce = Duration::ZERO;
        for width in [120, 80] {
            app.screen = Screen::new(Box::new(io::sink()), width, 30);
            app.handle_event(key('t'));
            drive_preview(&mut app);
            let texts = if width >= 100 {
                assert!(app.preview_pin.is_some());
                preview_texts(&app)
            } else {
                let Mode::Pager { lines, .. } = &app.mode else { panic!("expected pager") };
                lines.iter().map(|line| line.text()).collect()
            };
            assert_eq!(texts[0], folder.to_string_lossy(), "{texts:?}");
            // --dirsfirst: the nested directory comes before inside.txt.
            assert!(texts[1].ends_with("nested") && texts[2].ends_with("inner.txt"), "{texts:?}");
            assert!(texts.last().unwrap().ends_with("2 files"), "{texts:?}");
            assert!(!texts.iter().any(|line| line.ends_with("alpha")), "{texts:?}");
            app.handle_event(special(KeyCode::Esc));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn builtin_tree_plugin_uses_current_directory_when_empty() {
        if !crate::plugin::which("tree") {
            eprintln!("skipping: tree not installed");
            return;
        }
        let (mut app, d) = test_app("treeempty");
        std::fs::create_dir_all(d.join("empty")).unwrap();
        app.cd(&d.join("empty"));
        assert!(app.dir.hovered_path().is_none());
        app.preview.debounce = Duration::ZERO;
        app.handle_event(key('t'));
        drive_preview(&mut app);
        let texts = preview_texts(&app);
        assert_eq!(texts[0], app.dir.path.to_string_lossy(), "{texts:?}");
        assert!(texts.last().unwrap().ends_with("0 files"), "{texts:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pager_end_keeps_a_full_page() {
        let (mut app, d) = test_app("pagerend"); // 30 rows: 28-line pager body
        let lines: Vec<Line> = (0..100).map(|i| Line::plain(i.to_string())).collect();
        app.mode = Mode::Pager { lines, title: "t".into(), scroll: 0 };
        act(&mut app, Action::End);
        let Mode::Pager { scroll, .. } = app.mode else { panic!() };
        assert_eq!(scroll, 100 - 28);
        act(&mut app, Action::Next); // already at the bottom
        let Mode::Pager { scroll, .. } = app.mode else { panic!() };
        assert_eq!(scroll, 100 - 28);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn key_config_override() {
        let mut m = default_keymap();
        apply_key_config(&mut m, &[("quit".into(), "Z".into())]);
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE)], Action::Quit);
        assert!(!m.contains_key(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)));
    }

    #[cfg(unix)]
    #[test]
    fn enter_opens_by_rule_kitim_or_editor_and_skips_unknown() {
        // Files must exist before App::new loads the directory.
        let d = tmpdir("openact");
        std::fs::write(d.join("doc.pdf"), b"%PDF-1.4 noise").unwrap();
        // Media files get binary heads: real ones aren't text either.
        std::fs::write(d.join("pic.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        std::fs::write(d.join("song.flac"), [0x66, 0x4c, 0x61, 0x43, 0, 1, 2]).unwrap();
        std::fs::write(d.join("t.txt"), b"plain text\n").unwrap();
        std::fs::write(d.join("blob.dat"), [0u8, 1, 2, 3]).unwrap();
        let src = TestSource::new();
        let screen = Screen::new(Box::new(io::sink()), 120, 30);
        let mut app = App::new(Config::default(), d.clone(), screen, Box::new(src)).unwrap();
        app.sel.file = d.join(".sel");
        app.cfg.openers = vec![crate::config::OpenerRule { patterns: vec!["*.pdf".into()], cmd: "kitpdf -z 2".into() }];
        let hover = |app: &mut App, want: &str| {
            while app.dir.hovered_name() != Some(want) {
                act(app, Action::Next);
            }
            app.dir.hovered_path().unwrap()
        };

        // A user rule wins, file name appended after its command.
        let path = hover(&mut app, "doc.pdf");
        assert_eq!(app.file_open(&path, "doc.pdf", false), FileOpen::Program("kitpdf -z 2 doc.pdf".into()));

        // Built-in kitim: needs graphics and kitim installed, else unknown.
        app.graphics = true;
        let path = hover(&mut app, "pic.png");
        assert_eq!(app.file_open(&path, "pic.png", true), FileOpen::Program("kitim --fullscreen pic.png".into()));
        let path = hover(&mut app, "song.flac");
        assert_eq!(app.file_open(&path, "song.flac", false), FileOpen::Nothing);
        app.graphics = false;
        let path = hover(&mut app, "pic.png");
        assert_eq!(app.file_open(&path, "pic.png", true), FileOpen::Nothing);
        app.graphics = true;

        // Text opens in the editor; an unknown binary does nothing.
        let path = hover(&mut app, "t.txt");
        assert_eq!(app.file_open(&path, "t.txt", false), FileOpen::Edit);
        let path = hover(&mut app, "blob.dat");
        assert_eq!(app.file_open(&path, "blob.dat", true), FileOpen::Nothing);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn preview_pane_open_at_startup_previews_the_first_entry() {
        let d = tmpdir("pstart");
        std::fs::write(d.join("a.txt"), b"first\n").unwrap();
        let mut cfg = Config::default();
        cfg.ui.preview = true;
        let screen = Screen::new(Box::new(Vec::new()), 120, 30);
        let app = App::new(cfg, d.clone(), screen, Box::new(TestSource::new())).unwrap();
        assert!(app.preview.is_busy(), "no preview requested before the first keypress");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn preview_targets_follow_hover() {
        let d = tmpdir("ptarget");
        std::fs::write(d.join("t.txt"), b"content\n").unwrap();
        let mut cfg = Config::default();
        cfg.ui.preview = true;
        cfg.previewers = vec![crate::config::PreviewerRule {
            patterns: vec!["*.txt".into()],
            cmd: "cat".into(),
            graphics: false,
        }];
        let src = TestSource::new();
        let screen = Screen::new(Box::new(Vec::new()), 120, 30);
        let mut app = App::new(cfg, d.clone(), screen, Box::new(src)).unwrap();
        app.preview.enabled = true;
        app.preview_on_hover();
        // t.txt sorts last among the tmp fixture entries; walk to it.
        while app.dir.hovered_name() != Some("t.txt") {
            act(&mut app, Action::Next);
        }
        app.preview_on_hover();
        assert!(app.preview.is_busy());
        // Drive the debounce to expiry and the builtin (no cmd resolution yet —
        // rule matched "cat", but job spawn is exercised in preview.rs tests).
        app.preview.debounce = Duration::ZERO;
        app.preview_on_hover();
        let _ = app.preview.tick(Instant::now());
        let _ = std::fs::remove_dir_all(&d);
    }
}
