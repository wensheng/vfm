//! Configuration: defaults + one TOML file (`config.toml`, parsed via the
//! non-serde `toml::Table` API to keep the dep budget small) + a few env
//! overrides. nnn ships no config file at all; vfm needs one only for
//! previewer/plugin/keymap rules — everything else has sane defaults.

use crate::filter::FilterMode;
use crate::sort::SortKey;
use crate::ui::{Color, Style, Theme};
use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct Config {
    pub ui: UiCfg,
    pub sort: SortCfg,
    pub filter: FilterCfg,
    pub preview: PreviewCfg,
    pub theme: Theme,
    /// Extension/filename globs → preview command, first match wins.
    pub previewers: Vec<PreviewerRule>,
    /// Extension/filename globs → fullscreen open command, first match wins.
    pub openers: Vec<OpenerRule>,
    /// Named action plugins.
    pub plugins: Vec<PluginDef>,
    /// Key remaps (action name → key description).
    pub keys: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct UiCfg {
    pub show_hidden: bool,
    pub detail: bool,
    /// Show file icons when the terminal is configured with a Nerd Font.
    pub nerd_font: bool,
    pub preview: bool,
    pub wrap_nav: bool,
    pub scrolloff: usize,
    pub time_type: crate::util::TimeType,
}

#[derive(Clone, Debug)]
pub struct SortCfg {
    pub key: SortKey,
    pub reverse: bool,
    pub dirs_first: bool,
}

#[derive(Clone, Debug)]
pub struct FilterCfg {
    pub mode: FilterMode,
    pub smartcase: bool,
}

#[derive(Clone, Debug)]
pub struct PreviewCfg {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub max_bytes: usize,
    /// The built-in kitim plugin: image and video previews, off by default.
    pub kitim: bool,
    /// Videos previewed by the kitim plugin play their sound.
    pub kitim_sound: bool,
    /// Highlight text previews with bat when it is installed.
    pub bat: bool,
}

#[derive(Clone, Debug)]
pub struct OpenerRule {
    pub patterns: Vec<String>,
    /// Runs fullscreen with the file's quoted name appended.
    pub cmd: String,
}

#[derive(Clone, Debug)]
pub struct PreviewerRule {
    pub patterns: Vec<String>,
    pub cmd: String,
    /// Prints kitty graphics frames, not text (see `graphics`). Only tried
    /// when the terminal answered the graphics probe.
    pub graphics: bool,
}

/// File types kitim previews: what the image crate decodes, plus video
/// containers FFmpeg plays. Globs are case-sensitive, hence both cases.
const KITIM_TYPES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi", "pnm", "ppm", "pgm", "pbm",
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "3gp",
];

/// Audio types kitim plays but can't preview: no picture to show.
const KITIM_AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma", "alac", "aiff", "aif"];

/// Whether Enter opens `name` in kitim: an image, video or audio extension,
/// in any case. kitim itself decides if the terminal can show it.
pub fn is_kitim_media(name: &str) -> bool {
    // Path::extension treats ".png" as a hidden file with no extension.
    let Some(ext) = std::path::Path::new(name).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    KITIM_TYPES.iter().chain(KITIM_AUDIO).any(|t| ext == *t)
}

/// The rule `[preview] kitim = true` adds: kitim's embedded preview mode,
/// which takes the path, width and height vfm appends. With `sound`, videos
/// play their audio.
pub fn kitim_rule(sound: bool) -> PreviewerRule {
    let patterns = KITIM_TYPES
        .iter()
        .flat_map(|ext| [format!("*.{ext}"), format!("*.{}", ext.to_uppercase())])
        .collect();
    let cmd = if sound { "kitim --sound --preview" } else { "kitim --preview" };
    PreviewerRule { patterns, cmd: cmd.into(), graphics: true }
}

#[derive(Clone, Debug)]
pub struct PluginDef {
    pub name: String,
    pub cmd: String,
    pub key: Option<String>,
    /// Programs that must be on $PATH; the plugin refuses to run otherwise.
    pub requires: Vec<String>,
}

/// Plugins vfm ships with. A `[[plugin]]` with the same `name` replaces one
/// (e.g. to change its key, or `key = ""` to unbind it).
fn default_plugins() -> Vec<PluginDef> {
    let (cmd, program) = if cfg!(windows) {
        // Use Windows' tree.com even when PATH contains another tree tool.
        let program = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32")
            .join("tree.com")
            .to_string_lossy()
            .into_owned();
        let cmd =
            format!(r#"> if defined VFM_PATH ("{program}" "%VFM_PATH%" /F /A) else ("{program}" "%VFM_DIR%" /F /A)"#);
        (cmd, program)
    } else {
        (r#"> tree -C --dirsfirst -L 3 "${VFM_PATH:-$VFM_DIR}""#.into(), "tree".into())
    };
    vec![PluginDef { name: "tree".into(), cmd, key: Some("t".into()), requires: vec![program] }]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiCfg {
                show_hidden: false,
                detail: true,
                nerd_font: false,
                preview: false,
                wrap_nav: true,
                scrolloff: crate::dir::SCROLLOFF,
                time_type: crate::util::TimeType::Modified,
            },
            sort: SortCfg { key: SortKey::Name, reverse: false, dirs_first: true },
            filter: FilterCfg { mode: FilterMode::Substring, smartcase: true },
            preview: PreviewCfg {
                enabled: true,
                debounce_ms: 40,
                max_bytes: 256 * 1024,
                kitim: false,
                kitim_sound: false,
                bat: true,
            },
            theme: Theme::default(),
            previewers: Vec::new(),
            openers: Vec::new(),
            plugins: default_plugins(),
            keys: Vec::new(),
        }
    }
}

/// Config + data directory ($XDG_CONFIG_HOME or ~/.config/vfm).
pub fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("VFM_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x);
        }
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("vfm");
        }
    }
    dirs_home().join(".config").join("vfm")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE")) // windows
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> (Config, Option<String>) {
        let mut cfg = Config::default();
        let file = match path {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                let d = config_dir().join("config.toml");
                d.exists().then_some(d)
            }
        };
        let Some(file) = file else { return (cfg, None) };
        match std::fs::read_to_string(&file) {
            Ok(text) => match text.parse::<toml::Table>() {
                Ok(t) => {
                    let errs = cfg.apply_table(&t);
                    (cfg, errs.into_iter().next()) // surface first issue only
                }
                Err(e) => (cfg, Some(format!("{}: {e}", file.display()))),
            },
            Err(e) => (cfg, Some(format!("{}: {e}", file.display()))),
        }
    }

    fn apply_table(&mut self, t: &toml::Table) -> Vec<String> {
        let mut errs = Vec::new();
        if let Some(ui) = t.get("ui").and_then(|v| v.as_table()) {
            get_bool(ui, "show_hidden", &mut self.ui.show_hidden);
            get_bool(ui, "detail", &mut self.ui.detail);
            get_bool(ui, "nerd_font", &mut self.ui.nerd_font);
            get_bool(ui, "preview", &mut self.ui.preview);
            get_bool(ui, "wrap_nav", &mut self.ui.wrap_nav);
            if let Some(v) = ui.get("scrolloff").and_then(|v| v.as_integer()) {
                self.ui.scrolloff = v.clamp(0, 10) as usize;
            }
            if let Some(v) = ui.get("time_type").and_then(|v| v.as_str()) {
                self.ui.time_type = match v {
                    "modified" => crate::util::TimeType::Modified,
                    "accessed" => crate::util::TimeType::Accessed,
                    "created" => crate::util::TimeType::Created,
                    _ => self.ui.time_type,
                };
            }
        }
        if let Some(s) = t.get("sort").and_then(|v| v.as_table()) {
            if let Some(v) = s.get("key").and_then(|v| v.as_str()) {
                if let Some(k) = SortKey::parse(v) {
                    self.sort.key = k;
                }
            }
            get_bool(s, "reverse", &mut self.sort.reverse);
            get_bool(s, "dirs_first", &mut self.sort.dirs_first);
        }
        if let Some(f) = t.get("filter").and_then(|v| v.as_table()) {
            if let Some(v) = f.get("mode").and_then(|v| v.as_str()) {
                if let Some(m) = FilterMode::parse(v) {
                    self.filter.mode = m;
                }
            }
            get_bool(f, "smartcase", &mut self.filter.smartcase);
        }
        if let Some(p) = t.get("preview").and_then(|v| v.as_table()) {
            get_bool(p, "enabled", &mut self.preview.enabled);
            if let Some(v) = p.get("debounce_ms").and_then(|v| v.as_integer()) {
                self.preview.debounce_ms = v.clamp(0, 1000) as u64;
            }
            if let Some(v) = p.get("max_bytes").and_then(|v| v.as_integer()) {
                self.preview.max_bytes = (v.max(1024) as usize).min(16 * 1024 * 1024);
            }
            get_bool(p, "kitim", &mut self.preview.kitim);
            get_bool(p, "kitim_sound", &mut self.preview.kitim_sound);
            get_bool(p, "bat", &mut self.preview.bat);
        }
        // [[previewer]] rules
        if let Some(rules) = t.get("previewer").and_then(|v| v.as_array()) {
            for r in rules {
                let Some(tbl) = r.as_table() else { continue };
                let Some(cmd) = tbl.get("run").and_then(|v| v.as_str()).map(String::from)
                else {
                    errs.push("[[previewer]] missing `run`".into());
                    continue;
                };
                let patterns: Vec<String> = tbl
                    .get("match")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if patterns.is_empty() {
                    errs.push("[[previewer]] missing `match`".into());
                    continue;
                }
                let mut graphics = false;
                get_bool(tbl, "graphics", &mut graphics);
                self.previewers.push(PreviewerRule { patterns, cmd, graphics });
            }
        }
        // After the user's rules: first match wins, so theirs take precedence.
        if self.preview.kitim {
            self.previewers.push(kitim_rule(self.preview.kitim_sound));
        }
        // [[opener]] definitions
        if let Some(rules) = t.get("opener").and_then(|v| v.as_array()) {
            for r in rules {
                let Some(tbl) = r.as_table() else { continue };
                let Some(cmd) = tbl.get("run").and_then(|v| v.as_str()).map(String::from) else {
                    errs.push("[[opener]] missing `run`".into());
                    continue;
                };
                let patterns: Vec<String> = tbl
                    .get("match")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if patterns.is_empty() {
                    errs.push("[[opener]] missing `match`".into());
                    continue;
                }
                self.openers.push(OpenerRule { patterns, cmd });
            }
        }
        // [[plugin]] definitions
        if let Some(rules) = t.get("plugin").and_then(|v| v.as_array()) {
            for r in rules {
                let Some(tbl) = r.as_table() else { continue };
                let (Some(name), Some(cmd)) = (
                    tbl.get("name").and_then(|v| v.as_str()).map(String::from),
                    tbl.get("cmd").and_then(|v| v.as_str()).map(String::from),
                ) else {
                    errs.push("[[plugin]] needs `name` and `cmd`".into());
                    continue;
                };
                let key = tbl.get("key").and_then(|v| v.as_str()).map(String::from);
                // `requires = "tree"` or `requires = ["git", "fzf"]`
                let requires = match tbl.get("requires") {
                    Some(toml::Value::String(p)) => vec![p.clone()],
                    Some(toml::Value::Array(a)) => {
                        a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
                    }
                    _ => Vec::new(),
                };
                let def = PluginDef { name, cmd, key, requires };
                match self.plugins.iter_mut().find(|p| p.name == def.name) {
                    Some(existing) => *existing = def,
                    None => self.plugins.push(def),
                }
            }
        }
        // [keys] remaps
        if let Some(keys) = t.get("keys").and_then(|v| v.as_table()) {
            for (k, v) in keys {
                if let Some(desc) = v.as_str() {
                    self.keys.push((k.clone(), desc.to_string()));
                }
            }
        }
        // [colors] overrides → theme
        if let Some(colors) = t.get("colors").and_then(|v| v.as_table()) {
            for (slot, v) in colors {
                let Some(name) = v.as_str() else { continue };
                let Some(style) = parse_color(name) else {
                    errs.push(format!("[colors.{slot}]: unknown color '{name}'"));
                    continue;
                };
                match color_slot(&mut self.theme, slot) {
                    Some(s) => *s = style,
                    None => errs.push(format!("[colors.{slot}]: unknown slot")),
                }
            }
        }
        errs
    }
}

fn get_bool(t: &toml::Table, key: &str, dst: &mut bool) {
    if let Some(v) = t.get(key).and_then(|v| v.as_bool()) {
        *dst = v;
    }
}

/// "blue", "1"-"8", "9"-"15", "0xRRGGBB" → Style (fg only).
fn parse_color(s: &str) -> Option<Style> {
    let c = match s {
        "default" => Color::Default,
        "black" => Color::Idx(0),
        "red" => Color::Idx(1),
        "green" => Color::Idx(2),
        "yellow" => Color::Idx(3),
        "blue" => Color::Idx(4),
        "magenta" => Color::Idx(5),
        "cyan" => Color::Idx(6),
        "white" => Color::Idx(7),
        other => {
            if let Ok(n) = other.parse::<u8>() {
                Color::Idx(n)
            } else {
                let hex = other.strip_prefix("0x").filter(|h| h.len() == 6)?;
                let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
                Color::Rgb(byte(0)?, byte(2)?, byte(4)?)
            }
        }
    };
    Some(Style::fg(c))
}

/// NNN_FCOLORS-style env override: `dir=blue:exec=green:...`. Applied after
/// the config file (env wins, nnn-style flag precedence).
pub fn apply_env_overrides(cfg: &mut Config) {
    apply_env(
        cfg,
        std::env::var("VFM_FCOLORS").ok().as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("VFM_NERD_FONT").ok().as_deref(),
    );
}

/// Pure core of [`apply_env_overrides`] (env vars as arguments, testable).
fn apply_env(cfg: &mut Config, fcolors: Option<&str>, no_color: bool, nerd_font: Option<&str>) {
    match nerd_font {
        Some("1" | "true") => cfg.ui.nerd_font = true,
        Some("0" | "false") => cfg.ui.nerd_font = false,
        _ => {}
    }
    for pair in fcolors.unwrap_or("").split(':') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let Some(style) = parse_color(v) else { continue };
        if let Some(slot) = color_slot(&mut cfg.theme, k) {
            *slot = style;
        }
    }
    if no_color {
        let t = &mut cfg.theme;
        for s in [&mut t.dir, &mut t.link, &mut t.orphan, &mut t.exec, &mut t.hardlink, &mut t.missing] {
            *s = Style::default();
        }
    }
}

/// Color slot by name — one table for both `[colors]` and `VFM_FCOLORS`.
fn color_slot<'a>(t: &'a mut Theme, name: &str) -> Option<&'a mut Style> {
    Some(match name {
        "dir" => &mut t.dir,
        "symlink" | "link" => &mut t.link,
        "orphan" => &mut t.orphan,
        "exec" => &mut t.exec,
        "hardlink" => &mut t.hardlink,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        apply_env(&mut c, Some("dir=red:link=2:bogus=blue:exec"), false, None);
        assert_eq!(c.theme.dir, Style::fg(Color::Idx(1)));
        assert_eq!(c.theme.link, Style::fg(Color::Idx(2)));
        // NO_COLOR applies on its own, without VFM_FCOLORS set.
        let mut c = Config::default();
        apply_env(&mut c, None, true, None);
        assert_eq!(c.theme.dir, Style::default());
        assert_eq!(c.theme.exec, Style::default());
    }

    #[test]
    fn nerd_font_is_opt_in_and_environment_overrides_config() {
        let mut cfg = Config::default();
        assert!(!cfg.ui.nerd_font);
        for enabled in [true, false] {
            let t = format!("[ui]\nnerd_font = {enabled}\n").parse().unwrap();
            assert!(cfg.apply_table(&t).is_empty());
            assert_eq!(cfg.ui.nerd_font, enabled);
        }
        for value in ["1", "true"] {
            cfg.ui.nerd_font = false;
            apply_env(&mut cfg, None, true, Some(value));
            assert!(cfg.ui.nerd_font); // NO_COLOR only affects colors.
        }
        for value in ["0", "false"] {
            cfg.ui.nerd_font = true;
            apply_env(&mut cfg, None, false, Some(value));
            assert!(!cfg.ui.nerd_font);
        }
        for enabled in [true, false] {
            cfg.ui.nerd_font = enabled;
            for value in [None, Some(""), Some("invalid")] {
                apply_env(&mut cfg, None, false, value);
                assert_eq!(cfg.ui.nerd_font, enabled);
            }
        }
    }

    #[test]
    fn parse_defaults_and_rules() {
        let mut cfg = Config::default();
        let t: toml::Table = r#"
[ui]
show_hidden = true
detail = false

[sort]
key = "mtime"
reverse = true

[[previewer]]
match = ["*.md", "*.rs"]
run = "bat --color=always"

[[plugin]]
name = "git status"
cmd = "| git status"

[keys]
quit = "Q"

[colors]
dir = "cyan"
"#
        .parse()
        .unwrap();
        let errs = cfg.apply_table(&t);
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.ui.show_hidden);
        assert!(!cfg.ui.detail);
        assert_eq!(cfg.sort.key, SortKey::Mtime);
        assert!(cfg.sort.reverse);
        assert_eq!(cfg.previewers.len(), 1);
        assert_eq!(cfg.previewers[0].cmd, "bat --color=always");
        // User plugins come after the built-in ones.
        assert_eq!(cfg.plugins.last().unwrap().name, "git status");
        assert_eq!(cfg.keys, vec![("quit".to_string(), "Q".to_string())]);
        assert_eq!(cfg.theme.dir.fg, Color::Idx(6));
    }

    #[test]
    fn builtin_tree_plugin_and_override() {
        let cfg = Config::default();
        let tree = cfg.plugins.iter().find(|p| p.name == "tree").unwrap();
        assert_eq!(tree.key.as_deref(), Some("t"));
        #[cfg(unix)]
        assert_eq!(tree.requires, ["tree"]);
        #[cfg(windows)]
        {
            assert_eq!(tree.requires.len(), 1);
            assert!(PathBuf::from(&tree.requires[0]).is_absolute());
            assert!(tree.requires[0].ends_with(r"\System32\tree.com"));
            assert!(tree.cmd.contains(&format!(r#""{}""#, tree.requires[0])));
        }
        assert!(tree.cmd.starts_with('>'));

        // Same name replaces the built-in (here: rebind + new command).
        let mut cfg = Config::default();
        let n = cfg.plugins.len();
        let t: toml::Table = r#"
[[plugin]]
name = "tree"
cmd = "> tree -a -L 1"
key = "T"
requires = "tree"

[[plugin]]
name = "fzf cd"
cmd = "! fzf"
requires = ["fzf", "find"]
"#
        .parse()
        .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert_eq!(cfg.plugins.len(), n + 1);
        let tree = cfg.plugins.iter().find(|p| p.name == "tree").unwrap();
        assert_eq!((tree.cmd.as_str(), tree.key.as_deref()), ("> tree -a -L 1", Some("T")));
        assert_eq!(cfg.plugins.last().unwrap().requires, ["fzf", "find"]);
    }

    #[test]
    fn kitim_plugin_is_off_until_enabled_and_yields_to_user_rules() {
        let cfg = Config::default();
        assert!(!cfg.preview.kitim && cfg.previewers.is_empty());

        let mut cfg = Config::default();
        let t: toml::Table = r#"
[preview]
kitim = true

[[previewer]]
match = ["*.png"]
run = "my-viewer"
graphics = true
"#
        .parse()
        .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert_eq!(cfg.previewers.len(), 2);
        assert_eq!(cfg.previewers[0].cmd, "my-viewer");
        assert!(cfg.previewers[0].graphics);
        let kitim = &cfg.previewers[1];
        assert_eq!((kitim.cmd.as_str(), kitim.graphics), ("kitim --preview", true));
        assert!(!cfg.preview.kitim_sound);
        for name in ["*.jpg", "*.JPG", "*.mp4", "*.MOV", "*.gif"] {
            assert!(kitim.patterns.iter().any(|p| p == name), "{name}");
        }
    }

    #[test]
    fn kitim_sound_adds_the_sound_flag() {
        let mut cfg = Config::default();
        let t: toml::Table = "[preview]\nkitim = true\nkitim_sound = true\n".parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert_eq!(cfg.previewers.last().unwrap().cmd, "kitim --sound --preview");
        // Without the plugin enabled, the sound switch adds nothing.
        let mut cfg = Config::default();
        let t: toml::Table = "[preview]\nkitim_sound = true\n".parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.previewers.is_empty());
    }

    #[test]
    fn bat_highlighting_is_on_unless_disabled() {
        assert!(Config::default().preview.bat);
        let mut cfg = Config::default();
        let t: toml::Table = "[preview]\nbat = false\n".parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(!cfg.preview.bat);
    }

    #[test]
    fn opener_rules_parse_and_kitim_media_types() {
        let mut cfg = Config::default();
        let t: toml::Table = r#"
[[opener]]
match = ["*.pdf", "*.PDF"]
run = "kitpdf -z 2"

[[opener]]
run = "no patterns"
"#
        .parse()
        .unwrap();
        let errs = cfg.apply_table(&t);
        assert_eq!(cfg.openers.len(), 1);
        assert_eq!((cfg.openers[0].cmd.as_str(), cfg.openers[0].patterns.len()), ("kitpdf -z 2", 2));
        assert_eq!(errs, ["[[opener]] missing `match`"]);

        for name in ["a.PNG", "b.mp4", "c.MOV", "song.flac", "x.jpeg"] {
            assert!(is_kitim_media(name), "{name}");
        }
        for name in ["x.txt", "png", "a.mp4x", "archive.tar.gz", ".png"] {
            assert!(!is_kitim_media(name), "{name}");
        }
    }

    #[test]
    fn error_tolerant() {
        let mut cfg = Config::default();
        let t: toml::Table = "[ui]\nscrolloff = 99\n[colors]\ndir = \"nope\"\n".parse().unwrap();
        let _ = cfg.apply_table(&t);
        assert!(cfg.ui.scrolloff <= 10); // clamped, not crashed
    }
}
