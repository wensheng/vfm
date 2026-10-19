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
    /// Image and video previews through kitim, off by default.
    pub kitim: bool,
    /// Videos previewed by kitim play their sound.
    pub kitim_sound: bool,
    /// PDF and EPUB previews with page navigation through kitpdf.
    pub kitpdf: bool,
    /// An explicit Vivido profile replaces Kitty settings in Vivido and Vivida.
    pub vivido: bool,
    /// Highlight text previews with bat when it is installed.
    pub bat: bool,
    /// Show line numbers in bat text previews.
    pub line_numbers: bool,
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

/// File types kitim previews: what the image crate decodes, plus video
/// containers FFmpeg plays. Globs are case-sensitive, hence both cases.
const KITIM_TYPES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi", "pnm", "ppm", "pgm", "pbm",
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "3gp",
];

/// Audio types kitim plays but can't preview: no picture to show.
const KITIM_AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma", "alac", "aiff", "aif"];

/// Documents vvrd reads, in Vivid: kitpdf's types plus SVG and Markdown.
const VVRD_TYPES: &[&str] = &["pdf", "epub", "svg", "md", "markdown", "mkd"];

/// Whether Enter opens `name` in kitim (or vivi, in Vivid): an image, video
/// or audio extension, in any case. The viewer decides if it can show it.
pub fn is_kitim_media(name: &str) -> bool {
    has_extension(name, KITIM_TYPES.iter().chain(KITIM_AUDIO))
}

/// Whether Enter opens `name` in vvrd, in Vivid: a PDF, EPUB, SVG or
/// Markdown extension, in any case.
pub fn is_vvrd_document(name: &str) -> bool {
    has_extension(name, VVRD_TYPES.iter())
}

fn has_extension<'a>(name: &str, mut types: impl Iterator<Item = &'a &'a str>) -> bool {
    // Path::extension treats ".png" as a hidden file with no extension.
    let Some(ext) = std::path::Path::new(name).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    types.any(|t| ext == *t)
}

/// `*.ext` and `*.EXT` for each type: globs are case-sensitive.
fn both_cases(types: &[&str]) -> Vec<String> {
    types.iter().flat_map(|ext| [format!("*.{ext}"), format!("*.{}", ext.to_uppercase())]).collect()
}

/// The rule `[preview] kitim = true` adds: kitim's embedded preview mode,
/// which takes the path, width and height vfm appends. With `sound`, videos
/// play their audio.
pub fn kitim_rule(sound: bool) -> PreviewerRule {
    let cmd = if sound { "kitim --sound --preview" } else { "kitim --preview" };
    let patterns = both_cases(KITIM_TYPES);
    PreviewerRule {
        patterns,
        cmd: cmd.into(),
        scope: PreviewerScope::Kitty,
        graphics: true,
        vivid: false,
        interactive: false,
    }
}

/// Vivi's pane mode for images and videos in the Vivido profile.
///
/// It takes the pane's grid position after its size.
pub fn vivi_rule(sound: bool) -> PreviewerRule {
    let cmd = if sound { "vivi --sound --preview" } else { "vivi --preview" };
    let patterns = both_cases(KITIM_TYPES);
    PreviewerRule {
        patterns,
        cmd: cmd.into(),
        scope: PreviewerScope::Vivido,
        graphics: false,
        vivid: true,
        interactive: false,
    }
}

/// The built-in PDF/EPUB previewer, enabled with `[preview] kitpdf = true`.
pub fn kitpdf_rule() -> PreviewerRule {
    PreviewerRule {
        patterns: ["*.pdf", "*.PDF", "*.epub", "*.EPUB"].map(String::from).into(),
        cmd: "kitpdf --preview".into(),
        scope: PreviewerScope::Kitty,
        graphics: true,
        vivid: false,
        interactive: true,
    }
}

/// Vvrd's pane mode for PDF, EPUB, SVG, and Markdown in the Vivido profile.
pub fn vvrd_rule() -> PreviewerRule {
    PreviewerRule {
        patterns: both_cases(VVRD_TYPES),
        cmd: "vvrd --preview".into(),
        scope: PreviewerScope::Vivido,
        graphics: false,
        vivid: true,
        interactive: true,
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
                kitim: false,
                kitim_sound: false,
                kitpdf: false,
                vivido: false,
                bat: true,
                line_numbers: true,
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
            get_bool(p, "kitim", &mut self.preview.kitim);
            get_bool(p, "kitim_sound", &mut self.preview.kitim_sound);
            get_bool(p, "kitpdf", &mut self.preview.kitpdf);
            get_bool(p, "bat", &mut self.preview.bat);
            get_bool(p, "line_numbers", &mut self.preview.line_numbers);
            // Explicit profiles precede shared rules. A missing Vivido profile
            // inherits Kitty rules; even an empty Vivido table opts out.
            self.preview.vivido = p.contains_key("vivido");
            if let Some(value) = p.get("vivido") {
                if let Some(v) = value.as_table() {
                    self.previewers.extend(previewer_rules(
                        v,
                        "preview.vivido.previewer",
                        PreviewerScope::Vivido,
                        &mut errs,
                    ));
                    let mut vivi = false;
                    let mut sound = false;
                    let mut vvrd = false;
                    get_bool(v, "vivi", &mut vivi);
                    get_bool(v, "vivi_sound", &mut sound);
                    get_bool(v, "vvrd", &mut vvrd);
                    if vivi {
                        self.previewers.push(vivi_rule(sound));
                    }
                    if vvrd {
                        self.previewers.push(vvrd_rule());
                    }
                } else {
                    errs.push("[preview.vivido] must be a table".into());
                }
            }
            if let Some(value) = p.get("kitty") {
                if let Some(k) = value.as_table() {
                    self.previewers.extend(previewer_rules(
                        k,
                        "preview.kitty.previewer",
                        PreviewerScope::Kitty,
                        &mut errs,
                    ));
                    get_bool(k, "kitim", &mut self.preview.kitim);
                    get_bool(k, "kitim_sound", &mut self.preview.kitim_sound);
                    get_bool(k, "kitpdf", &mut self.preview.kitpdf);
                    if self.preview.kitim {
                        self.previewers.push(kitim_rule(self.preview.kitim_sound));
                    }
                    if self.preview.kitpdf {
                        self.previewers.push(kitpdf_rule());
                    }
                } else {
                    errs.push("[preview.kitty] must be a table".into());
                }
            }
        }
        self.previewers.extend(previewer_rules(
            t,
            "previewer",
            PreviewerScope::Shared,
            &mut errs,
        ));
        // Legacy switches remain Kitty defaults, after the user's rules.
        let kitty_profile = t.get("preview").and_then(|v| v.get("kitty")).is_some();
        if !kitty_profile {
            if self.preview.kitim {
                self.previewers.push(kitim_rule(self.preview.kitim_sound));
            }
            if self.preview.kitpdf {
                self.previewers.push(kitpdf_rule());
            }
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

/// Parses shared and terminal-specific rules with the same protocol checks.
fn previewer_rules(
    t: &toml::Table,
    section: &str,
    scope: PreviewerScope,
    errs: &mut Vec<String>,
) -> Vec<PreviewerRule> {
    let mut parsed = Vec::new();
    let Some(rules) = t.get("previewer").and_then(|v| v.as_array()) else {
        return parsed;
    };
    for r in rules {
        let Some(tbl) = r.as_table() else { continue };
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
        assert_eq!((kitim.cmd.as_str(), kitim.graphics, kitim.vivid), ("kitim --preview", true, false));
        assert_eq!(kitim.scope, PreviewerScope::Kitty);
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
        assert_eq!(cfg.previewers.len(), 1);
        // Without the plugin enabled, the sound switch adds nothing.
        let mut cfg = Config::default();
        let t: toml::Table = "[preview]\nkitim_sound = true\n".parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.previewers.is_empty());
    }

    #[test]
    fn kitpdf_plugin_is_opt_in_and_custom_rules_take_precedence() {
        let mut cfg = Config::default();
        assert!(!cfg.preview.kitpdf);
        let t: toml::Table = r#"
[preview]
kitpdf = true
[[previewer]]
match = ["*.pdf"]
run = "custom --preview"
graphics = true
interactive = true
"#
        .parse()
        .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert_eq!(cfg.previewers.len(), 2);
        assert_eq!(cfg.previewers[0].cmd, "custom --preview");
        let rule = &cfg.previewers[1];
        assert_eq!(rule.cmd, "kitpdf --preview");
        assert!(rule.graphics && rule.interactive && !rule.vivid);
        assert_eq!(rule.patterns, ["*.pdf", "*.PDF", "*.epub", "*.EPUB"]);
        let t: toml::Table = "[[previewer]]\nmatch=['*.pdf']\nrun='bad'\ninteractive=true".parse().unwrap();
        assert!(!Config::default().apply_table(&t).is_empty());
        let t: toml::Table = "[[previewer]]\nmatch=['*.pdf']\nrun='both'\ngraphics=true\nvivid=true".parse().unwrap();
        assert!(!Config::default().apply_table(&t).is_empty());
        let mut cfg = Config::default();
        let mine = "[[previewer]]\nmatch=['*.pdf']\nrun='mine'\nvivid=true\ninteractive=true";
        let t: toml::Table = mine.parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.previewers[0].vivid && cfg.previewers[0].interactive);
    }

    #[test]
    fn terminal_profiles_add_only_their_explicit_viewers() {
        let mut cfg = Config::default();
        let t = r#"
[preview.kitty]
kitim = true
kitim_sound = true
kitpdf = true

[preview.vivido]
vivi = true
vivi_sound = true
vvrd = true
"#
        .parse()
        .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(cfg.preview.vivido);
        assert_eq!(cfg.previewers.len(), 4);
        let vivi = &cfg.previewers[0];
        assert_eq!(vivi.cmd, "vivi --sound --preview");
        assert!(vivi.vivid && !vivi.graphics && !vivi.interactive);
        assert_eq!(vivi.scope, PreviewerScope::Vivido);
        assert_eq!(vivi.patterns, cfg.previewers[2].patterns);
        let vvrd = &cfg.previewers[1];
        assert_eq!(vvrd.cmd, "vvrd --preview");
        assert!(vvrd.vivid && vvrd.interactive && !vvrd.graphics);
        for name in ["*.pdf", "*.EPUB", "*.svg", "*.SVG", "*.md", "*.markdown"] {
            assert!(vvrd.patterns.iter().any(|p| p == name), "{name}");
        }
        assert_eq!(cfg.previewers[2].cmd, "kitim --sound --preview");
        assert_eq!(cfg.previewers[3].cmd, "kitpdf --preview");
    }

    #[test]
    fn kitty_profile_overrides_legacy_switches_without_duplicate_rules() {
        let mut cfg = Config::default();
        let t = "[preview]\nkitim=true\nkitpdf=true\n[preview.kitty]\nkitim=false\n"
            .parse()
            .unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert_eq!(cfg.previewers.len(), 1);
        assert_eq!(cfg.previewers[0].cmd, "kitpdf --preview");
        assert!(!cfg.preview.vivido);
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
    fn bat_highlighting_is_on_unless_disabled() {
        assert!(Config::default().preview.bat);
        let mut cfg = Config::default();
        let t: toml::Table = "[preview]\nbat = false\n".parse().unwrap();
        assert!(cfg.apply_table(&t).is_empty());
        assert!(!cfg.preview.bat);
    }

    #[test]
    fn preview_line_numbers_default_on_and_follow_config() {
        let mut cfg = Config::default();
        assert!(cfg.preview.line_numbers);
        for enabled in [false, true] {
            let t: toml::Table = format!("[preview]\nline_numbers = {enabled}\n").parse().unwrap();
            assert!(cfg.apply_table(&t).is_empty());
            assert_eq!(cfg.preview.line_numbers, enabled);
            assert!(cfg.preview.bat);
        }
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
        for name in ["a.PDF", "b.epub", "c.svg", "README.md", "notes.Markdown"] {
            assert!(is_vvrd_document(name), "{name}");
        }
        for name in ["a.png", "x.txt", ".md", "svg"] {
            assert!(!is_vvrd_document(name), "{name}");
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
