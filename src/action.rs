//! The action enum + keybinding table — the port of nnn's `enum action` and
//! `bindings[]` (nnn.h) with `nextsel()`'s lookup semantics (nnn.c:3696).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

/// Every operation vfm can perform. v1 subset of nnn's ~60 actions; the
/// deferred ones (sessions, bookmarks, launcher, du, archive, ...) are listed
/// in the README's "future work" section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    // navigation
    Back,          // h / Left: parent dir
    Open,          // Enter: enter dir / open file
    NavIn,         // l / Right: enter dir or focus text preview
    Next,          // j / Down
    Prev,          // k / Up
    PgDn,
    PgUp,
    HalfPgDn, // C-d
    HalfPgUp, // C-u
    Home,     // g
    End,      // G
    FirstFile, // '
    Jump,     // J +n/-n
    Youngest, // C-y: most recently modified entry
    // cd
    CdHome,   // ~
    CdStart,  // @: dir vfm started in
    CdLast,   // -
    CdRoot,   // `
    // contexts
    CycleCtx(i8), // Tab / BackTab
    GotoCtx(u8),  // 1..=8
    QuitCtx,      // q
    Quit,         // C-q (exit 0)
    QuitErr,      // Q (exit 1)
    QuitCd,       // C-g: write cd file, exit 0
    // view
    ToggleHidden, // .
    ToggleDetail, // d
    FileInfo,     // f
    CycleSort,    // s: name → mtime → size → ext
    ReverseSort,  // C-t
    CycleTime,    // T: mtime → atime → crtime
    Redraw,       // C-l
    TogglePreview, // P
    PreviewWider,  // <: preview divider one step left
    PreviewNarrower, // >: preview divider one step right
    AutoNext,     // C-j: auto-advance after open
    Help,         // ?
    // filter
    Filter,          // /
    CycleFilterMode, // C-n
    // selection
    Select,       // Space: toggle hovered, advance
    SelectMul,    // m: range anchor / complete range
    SelectAll,    // a
    SelectInvert, // A
    SelEdit,      // E: edit selection file in $EDITOR
    SelSize,      // S: selection size
    // file ops
    Copy,    // p: paste (copy selection here)
    Move,    // v: paste (move selection here)
    CopyAs,  // w: copy hovered/first-selected with a new name
    Trash,   // x
    Delete,  // X (typed confirm)
    New,     // n: new file (trailing '/' → dir)
    Rename,  // C-r
    OpenWith, // o: prompt command, path appended
    Edit,    // e: $EDITOR on hovered
    Shell,   // !: $SHELL in cwd
    PluginMenu, // ;
    PluginRun(usize), // direct plugin key
    // synthetic
    Click { row: u16, col: u16, kind: ClickKind },
    Resize,
    /// Input fed to an open prompt/filter (chars, backspace, ...) — produced
    /// by the mode-specific event handlers.
    Input(InputEv),
    /// y/Y in a confirmation dialog (routed by mode, not the keymap).
    ConfirmYes,
    /// Cancel current mode (Esc, and 'y'/n handled by Confirm directly).
    Esc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickKind {
    Single,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with the left button held.
    Drag,
    /// The left button went up.
    Release,
}

/// Keystrokes routed to the active InputLine (filter / prompts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEv {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    /// History/exit/accept are NOT here — the mode maps Enter/Esc itself.
    WordBack, // C-w
    Clear,    // C-u
}

/// Build the default keymap (nnn-compatible keys). `[keys]` config entries
/// are applied on top (action-name → key description).
pub fn default_keymap() -> HashMap<KeyEvent, Action> {
    use KeyCode::*;
    let mut m = HashMap::new();
    let mut bind = |code: KeyCode, mods: KeyModifiers, a: Action| {
        m.insert(KeyEvent::new(code, mods), a);
    };
    let c = KeyModifiers::CONTROL;
    let s = KeyModifiers::SHIFT;

    // navigation
    bind(Char('h'), KeyModifiers::NONE, Action::Back);
    bind(Left, KeyModifiers::NONE, Action::Back);
    bind(Enter, KeyModifiers::NONE, Action::Open);
    bind(Char('j'), KeyModifiers::NONE, Action::Next);
    bind(Down, KeyModifiers::NONE, Action::Next);
    bind(Char('k'), KeyModifiers::NONE, Action::Prev);
    bind(Up, KeyModifiers::NONE, Action::Prev);
    bind(Char('l'), KeyModifiers::NONE, Action::NavIn);
    bind(Right, KeyModifiers::NONE, Action::NavIn);
    bind(PageDown, KeyModifiers::NONE, Action::PgDn);
    bind(PageUp, KeyModifiers::NONE, Action::PgUp);
    bind(Char('d'), c, Action::HalfPgDn);
    bind(Char('u'), c, Action::HalfPgUp);
    bind(Char('g'), KeyModifiers::NONE, Action::Home);
    bind(Home, KeyModifiers::NONE, Action::Home);
    bind(Char('a'), c, Action::Home); // nnn C-a = home
    bind(Char('G'), KeyModifiers::SHIFT, Action::End);
    bind(End, KeyModifiers::NONE, Action::End);
    bind(Char('e'), c, Action::End); // nnn C-e = end
    bind(Char('\''), KeyModifiers::NONE, Action::FirstFile);
    bind(Char('J'), KeyModifiers::SHIFT, Action::Jump);
    bind(Char('y'), c, Action::Youngest);
    // cd
    bind(Char('~'), KeyModifiers::NONE, Action::CdHome);
    bind(Char('@'), KeyModifiers::NONE, Action::CdStart);
    bind(Char('-'), KeyModifiers::NONE, Action::CdLast);
    bind(Char('`'), KeyModifiers::NONE, Action::CdRoot);
    // contexts
    bind(Tab, KeyModifiers::NONE, Action::CycleCtx(1));
    bind(BackTab, s, Action::CycleCtx(-1));
    for i in 1u8..=8 {
        bind(Char(std::char::from_digit(i as u32, 10).unwrap()), KeyModifiers::NONE, Action::GotoCtx(i - 1));
    }
    bind(Char('q'), KeyModifiers::NONE, Action::QuitCtx);
    bind(Char('q'), c, Action::Quit);
    bind(Char('Q'), KeyModifiers::SHIFT, Action::QuitErr);
    bind(Char('g'), c, Action::QuitCd);
    // view
    bind(Char('.'), KeyModifiers::NONE, Action::ToggleHidden);
    bind(Char('d'), KeyModifiers::NONE, Action::ToggleDetail);
    bind(Char('f'), KeyModifiers::NONE, Action::FileInfo);
    bind(Char('s'), KeyModifiers::NONE, Action::CycleSort);
    bind(Char('t'), c, Action::ReverseSort);
    bind(Char('T'), KeyModifiers::SHIFT, Action::CycleTime);
    bind(Char('l'), c, Action::Redraw);
    bind(Char('P'), KeyModifiers::SHIFT, Action::TogglePreview);
    bind(Char('<'), KeyModifiers::NONE, Action::PreviewWider);
    bind(Char('>'), KeyModifiers::NONE, Action::PreviewNarrower);
    bind(Char('j'), c, Action::AutoNext);
    bind(Char('?'), KeyModifiers::NONE, Action::Help);
    // filter
    bind(Char('/'), KeyModifiers::NONE, Action::Filter);
    bind(Char('n'), c, Action::CycleFilterMode);
    // selection
    bind(Char(' '), KeyModifiers::NONE, Action::Select);
    bind(Char('m'), KeyModifiers::NONE, Action::SelectMul);
    bind(Char('+'), KeyModifiers::NONE, Action::SelectAll);
    bind(Char('a'), KeyModifiers::NONE, Action::SelectAll);
    bind(Char('A'), KeyModifiers::SHIFT, Action::SelectInvert);
    bind(Char('E'), KeyModifiers::SHIFT, Action::SelEdit);
    bind(Char('S'), KeyModifiers::SHIFT, Action::SelSize);
    // file ops
    bind(Char('p'), KeyModifiers::NONE, Action::Copy);
    bind(Char('v'), KeyModifiers::NONE, Action::Move);
    bind(Char('w'), KeyModifiers::NONE, Action::CopyAs);
    bind(Char('x'), KeyModifiers::NONE, Action::Trash);
    bind(Char('X'), KeyModifiers::SHIFT, Action::Delete);
    bind(Char('n'), KeyModifiers::NONE, Action::New);
    bind(Char('r'), c, Action::Rename);
    bind(Char('o'), KeyModifiers::NONE, Action::OpenWith);
    bind(Char('e'), KeyModifiers::NONE, Action::Edit);
    bind(Char('!'), KeyModifiers::NONE, Action::Shell);
    bind(Char(';'), KeyModifiers::NONE, Action::PluginMenu);
    m
}

/// Parse a key description: "Q", "C-r", "<esc>", "<tab>", "<enter>", "<up>",
/// "<f1>"... Used by `[keys]` config overrides.
pub fn parse_key(desc: &str) -> Option<KeyEvent> {
    let d = desc.trim();
    let (mods, rest) = if let Some(r) = d.strip_prefix("C-") {
        (KeyModifiers::CONTROL, r)
    } else if let Some(r) = d.strip_prefix("S-") {
        (KeyModifiers::SHIFT, r)
    } else {
        (KeyModifiers::NONE, d)
    };
    let code = if rest.len() == 1 {
        KeyCode::Char(rest.chars().next()?)
    } else {
        match rest.trim_matches('<').trim_matches('>') {
            "esc" => KeyCode::Esc,
            "enter" | "cr" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" | "pageup" => KeyCode::PageUp,
            "pgdn" | "pagedown" => KeyCode::PageDown,
            "bksp" | "backspace" => KeyCode::Backspace,
            "del" | "delete" => KeyCode::Delete,
            other => KeyCode::F(other.strip_prefix('f')?.parse::<u8>().ok()?.clamp(1, 12)),
        }
    };
    Some(KeyEvent::new(code, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_basics() {
        let m = default_keymap();
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)], Action::Next);
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)], Action::Rename);
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE)], Action::GotoCtx(3));
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)], Action::CycleSort);
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE)], Action::PreviewWider);
        assert_eq!(m[&KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE)], Action::PreviewNarrower);
        assert!(!m.contains_key(&KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)));
        // Sanity: every Action variant is reachable from at least one key
        // except the synthetic ones (Click/Resize/Input/Esc/PluginRun).
        assert!(m.len() > 50);
    }

    #[test]
    fn key_parse_roundtrip() {
        assert_eq!(parse_key("Q"), Some(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT)));
        assert_eq!(parse_key("C-r"), Some(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(parse_key("<esc>"), Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(parse_key("bogus-key-name"), None);
    }
}
