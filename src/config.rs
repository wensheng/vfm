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
    /// Named action plugins.
    pub plugins: Vec<PluginDef>,
    /// Key remaps (action name → key description).
    pub keys: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct UiCfg {
    pub show_hidden: bool,
    pub detail: bool,
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
}

#[derive(Clone, Debug)]
pub struct PreviewerRule {
    pub patterns: Vec<String>,
    pub cmd: String,
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
    // Windows ships its own tree.com with different flags (no colors).
    let cmd = if cfg!(windows) {
        r#"> if defined VFM_PATH (tree "%VFM_PATH%" /F /A) else (tree /F /A)"#
    } else {
        r#"> tree -C --dirsfirst -L 3 "${VFM_PATH:-$VFM_DIR}""#
    };
    vec![PluginDef {
        name: "tree".into(),
        cmd: cmd.into(),
        key: Some("s".into()),
        requires: vec!["tree".into()],
    }]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiCfg {
                show_hidden: false,
                detail: true,
                preview: false,
                wrap_nav: true,
                scrolloff: crate::dir::SCROLLOFF,
                time_type: crate::util::TimeType::Modified,
            },
            sort: SortCfg { key: SortKey::Name, reverse: false, dirs_first: true },
            filter: FilterCfg { mode: FilterMode::Substring, smartcase: true },
            preview: PreviewCfg { enabled: true, debounce_ms: 40, max_bytes: 256 * 1024 },
            theme: Theme::default(),
            previewers: Vec::new(),
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
                self.previewers.push(PreviewerRule { patterns, cmd });
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
    apply_env(cfg, std::env::var("VFM_FCOLORS").ok().as_deref(), std::env::var_os("NO_COLOR").is_some());
}

/// Pure core of [`apply_env_overrides`] (env vars as arguments, testable).
fn apply_env(cfg: &mut Config, fcolors: Option<&str>, no_color: bool) {
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
        apply_env(&mut c, Some("dir=red:link=2:bogus=blue:exec"), false);
        assert_eq!(c.theme.dir, Style::fg(Color::Idx(1)));
        assert_eq!(c.theme.link, Style::fg(Color::Idx(2)));
        // NO_COLOR applies on its own, without VFM_FCOLORS set.
        let mut c = Config::default();
        apply_env(&mut c, None, true);
        assert_eq!(c.theme.dir, Style::default());
        assert_eq!(c.theme.exec, Style::default());
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
        assert_eq!(tree.key.as_deref(), Some("s"));
        assert_eq!(tree.requires, ["tree"]);
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
    fn error_tolerant() {
        let mut cfg = Config::default();
        let t: toml::Table = "[ui]\nscrolloff = 99\n[colors]\ndir = \"nope\"\n".parse().unwrap();
        let _ = cfg.apply_table(&t);
        assert!(cfg.ui.scrolloff <= 10); // clamped, not crashed
    }
}
