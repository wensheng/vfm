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
    /// The preview pane's share of the terminal width, in percent.
    pub preview_width: u16,
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
    /// An explicit Vivido profile replaces Kitty settings in Vivido and Vivida.
    pub vivido: bool,
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
    /// Terminal profile containing this rule, independent of its output protocol.
    pub scope: PreviewerScope,
    /// Prints kitty graphics frames, not text (see `graphics`). Only tried
    /// when the terminal answered the graphics probe.
    pub graphics: bool,
    /// Draws through the Vivid side channel itself, at the pane's grid
    /// position, so vfm appends that too. Only tried inside Vivid. Gets
    /// `hide`/`show` lines on stdin while something covers the pane.
    pub vivid: bool,
    /// Accepts next/prev/first/last lines on stdin while focused.
    pub interactive: bool,
}

/// Terminals eligible for a configured previewer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewerScope {
    /// Shared rules from top-level `[[previewer]]` entries.
    #[default]
    Shared,
    /// Kitty graphics terminals, including Vivido without its own profile.
    Kitty,
    /// Vivido and Vivida only.
    Vivido,
}

impl PreviewerRule {
    /// Checks terminal profile selection and the required output protocol.
    pub fn supports(&self, term: crate::term::Kind, graphics: bool, vivido_profile: bool) -> bool {
        let vivid = term == crate::term::Kind::Vivid;
        let selected = match self.scope {
            PreviewerScope::Shared => true,
            PreviewerScope::Kitty => graphics && !(vivid && vivido_profile),
            PreviewerScope::Vivido => vivid,
        };
        selected && (graphics || !self.graphics) && (vivid || !self.vivid)
    }
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
                preview_width: 50,
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
                vivido: false,
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
            if let Some(v) = ui.get("preview_width").and_then(|v| v.as_integer()) {
                self.ui.preview_width = v.clamp(10, 90) as u16;
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
            check_keys(
                p,
                "preview",
                &["enabled", "debounce_ms", "max_bytes", "kitty", "vivido"],
                &mut errs,
            );
            // Explicit profiles precede shared rules. A missing Vivido profile
            // inherits Kitty rules; even an empty Vivido table opts out.
            self.preview.vivido = p.contains_key("vivido");
            for (name, scope) in [
                ("vivido", PreviewerScope::Vivido),
                ("kitty", PreviewerScope::Kitty),
            ] {
                if let Some(value) = p.get(name) {
                    let section = format!("preview.{name}");
                    if let Some(profile) = value.as_table() {
                        check_keys(profile, &section, &["previewer"], &mut errs);
                        self.previewers.extend(previewer_rules(
                            profile,
                            &format!("{section}.previewer"),
                            scope,
                            &mut errs,
                        ));
                    } else {
                        errs.push(format!("[{section}] must be a table"));
                    }
                }
            }
        }
        self.previewers.extend(previewer_rules(
            t,
            "previewer",
            PreviewerScope::Shared,
            &mut errs,
        ));
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

/// Parses shared and terminal-specific rules with the same protocol checks.
fn previewer_rules(
    t: &toml::Table,
    section: &str,
    scope: PreviewerScope,
    errs: &mut Vec<String>,
) -> Vec<PreviewerRule> {
    let mut parsed = Vec::new();
    let Some(value) = t.get("previewer") else {
        return parsed;
    };
    let Some(rules) = value.as_array() else {
        errs.push(format!("[[{section}]] must be an array of tables"));
        return parsed;
    };
    for r in rules {
        let Some(tbl) = r.as_table() else {
            errs.push(format!("[[{section}]] must contain tables"));
            continue;
        };
        let Some(cmd) = tbl.get("run").and_then(|v| v.as_str()).map(String::from) else {
            errs.push(format!("[[{section}]] missing `run`"));
            continue;
        };
        let patterns: Vec<String> = tbl
            .get("match")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if patterns.is_empty() {
            errs.push(format!("[[{section}]] missing `match`"));
            continue;
        }
        let mut graphics = false;
        get_bool(tbl, "graphics", &mut graphics);
        let mut vivid = false;
        get_bool(tbl, "vivid", &mut vivid);
        let mut interactive = false;
        get_bool(tbl, "interactive", &mut interactive);
        if graphics && vivid {
            errs.push("a previewer is either graphics = true or vivid = true".into());
            continue;
        }
        if interactive && !graphics && !vivid {
            errs.push("interactive previewers require graphics = true or vivid = true".into());
            continue;
        }
        parsed.push(PreviewerRule {
            patterns,
            cmd,
            scope,
            graphics,
            vivid,
            interactive,
        });
    }
    parsed
}

/// Reports unsupported settings without assigning meaning to program names.
fn check_keys(t: &toml::Table, section: &str, allowed: &[&str], errs: &mut Vec<String>) {
    for key in t.keys() {
        if !allowed.contains(&key.as_str()) {
            errs.push(format!("[{section}]: unknown setting `{key}`"));
        }
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
run = "text-renderer --color=always"

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
        assert_eq!(cfg.previewers[0].cmd, "text-renderer --color=always");
        // User plugins come after the built-in ones.
        assert_eq!(cfg.plugins.last().unwrap().name, "git status");
        assert_eq!(cfg.keys, vec![("quit".to_string(), "Q".to_string())]);
        assert_eq!(cfg.theme.dir.fg, Color::Idx(6));
    }

    #[test]
    fn preview_width_defaults_to_half_and_is_clamped() {
        assert_eq!(Config::default().ui.preview_width, 50);
        for (value, want) in [(35, 35), (0, 10), (-4, 10), (95, 90)] {
            let mut cfg = Config::default();
            let t = format!("[ui]\npreview_width = {value}\n").parse().unwrap();
            assert!(cfg.apply_table(&t).is_empty());
            assert_eq!(cfg.ui.preview_width, want, "preview_width = {value}");
        }
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
    fn previews_have_no_external_commands_until_configured() {
        let cfg = Config::default();
        assert!(cfg.previewers.is_empty());
        assert!(cfg.openers.is_empty());
        assert!(!cfg.preview.vivido);

        let mut cfg = Config::default();
        let t = r#"
[[preview.kitty.previewer]]
match = ["*.custom"]
run = "any-program --sound --custom-flag"
graphics = true
interactive = true

[[preview.vivido.previewer]]
match = ["*.something-else"]
run = "/custom/path/to/a-program --anything"
vivid = true
"#
        .parse()
        .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.preview.vivido);
        assert_eq!(cfg.previewers.len(), 2);
        assert_eq!(cfg.previewers[0].patterns, ["*.something-else"]);
        assert_eq!(
            cfg.previewers[0].cmd,
            "/custom/path/to/a-program --anything"
        );
        assert_eq!(cfg.previewers[0].scope, PreviewerScope::Vivido);
        assert!(cfg.previewers[0].vivid && !cfg.previewers[0].graphics);
        assert_eq!(cfg.previewers[1].patterns, ["*.custom"]);
        assert_eq!(cfg.previewers[1].cmd, "any-program --sound --custom-flag");
        assert_eq!(cfg.previewers[1].scope, PreviewerScope::Kitty);
        assert!(cfg.previewers[1].graphics && cfg.previewers[1].interactive);
    }

    #[test]
    fn preview_settings_reject_program_specific_switches_as_unknown_keys() {
        for section in ["preview", "preview.kitty", "preview.vivido"] {
            let mut cfg = Config::default();
            let t = format!("[{section}]\nmy_viewer = true\n").parse().unwrap();
            assert_eq!(
                cfg.apply_table(&t),
                [format!("[{section}]: unknown setting `my_viewer`")]
            );
            assert!(cfg.previewers.is_empty());
        }
    }

    #[test]
    fn previewer_lists_reject_invalid_shapes_in_every_scope() {
        for prefix in ["", "preview.kitty.", "preview.vivido."] {
            let section = format!("{prefix}previewer");
            for (value, error) in [
                ("{}", "must be an array of tables"),
                ("[false]", "must contain tables"),
            ] {
                let mut cfg = Config::default();
                let t = format!("{section} = {value}").parse().unwrap();
                assert_eq!(cfg.apply_table(&t), [format!("[[{section}]] {error}")]);
                assert!(cfg.previewers.is_empty());
            }
        }
    }

    #[test]
    fn example_config_defines_its_own_commands_and_file_patterns() {
        let mut cfg = Config::default();
        let t = include_str!("../config.example.toml").parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.preview.vivido);
        assert_eq!(cfg.previewers.len(), 5);
        assert!(
            cfg.previewers
                .iter()
                .all(|r| !r.patterns.is_empty() && !r.cmd.is_empty())
        );
    }

    #[test]
    fn scoped_previewer_rules_report_their_section_and_validate_protocols() {
        for section in ["preview.kitty", "preview.vivido"] {
            for (rule, error) in [
                (
                    "match=['*.pdf']",
                    format!("[[{section}.previewer]] missing `run`"),
                ),
                (
                    "run='viewer'",
                    format!("[[{section}.previewer]] missing `match`"),
                ),
                (
                    "match=['*.pdf']\nrun='viewer'\ngraphics=true\nvivid=true",
                    "a previewer is either graphics = true or vivid = true".into(),
                ),
                (
                    "match=['*.pdf']\nrun='viewer'\ninteractive=true",
                    "interactive previewers require graphics = true or vivid = true".into(),
                ),
            ] {
                let t = format!("[[{section}.previewer]]\n{rule}\n")
                    .parse()
                    .unwrap();
                assert_eq!(Config::default().apply_table(&t), [error]);
            }
        }
        for section in ["kitty", "vivido"] {
            let t = format!("[preview]\n{section}=false\n").parse().unwrap();
            assert_eq!(
                Config::default().apply_table(&t),
                [format!("[preview.{section}] must be a table")]
            );
        }
    }

    #[test]
    fn opener_rules_parse_arbitrary_commands_and_patterns() {
        let mut cfg = Config::default();
        let t: toml::Table = r#"
[[opener]]
match = ["*.pdf", "*.PDF"]
run = "document-reader --zoom 2"

[[opener]]
run = "no patterns"
"#
        .parse()
        .unwrap();
        let errs = cfg.apply_table(&t);
        assert_eq!(cfg.openers.len(), 1);
        assert_eq!(
            (cfg.openers[0].cmd.as_str(), cfg.openers[0].patterns.len()),
            ("document-reader --zoom 2", 2)
        );
        assert_eq!(errs, ["[[opener]] missing `match`"]);
    }

    #[test]
    fn error_tolerant() {
        let mut cfg = Config::default();
        let t: toml::Table = "[ui]\nscrolloff = 99\n[colors]\ndir = \"nope\"\n".parse().unwrap();
        let _ = cfg.apply_table(&t);
        assert!(cfg.ui.scrolloff <= 10); // clamped, not crashed
    }
}
