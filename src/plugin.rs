//! External command integration: `!` shell, `e`/`o`/`E` editor/open-with, and
//! the `[[plugin]]` system. Two spawn modes: fullscreen (TUI released, stdio
//! inherited, wait) and capture (`|` prefix: stdout piped, then scanned for
//! `vfm:` control lines — vfm's replacement for nnn's plugin FIFO).

use std::path::{Path, PathBuf};
use std::process::Command;

pub const CTL_PREFIX: &str = "vfm:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnMode {
    /// TUI suspended, child owns the terminal (interactive default).
    Fullscreen,
    /// `|` prefix: stdout captured, shown in a pager overlay.
    Capture,
    /// `>` prefix: stdout streamed into the preview pane (async, killable),
    /// pinned there until the user navigates.
    Preview,
}

/// Split the mode prefix off a plugin cmd. Default (no prefix) is fullscreen.
pub fn resolve_mode(cmd: &str) -> (SpawnMode, &str) {
    let cmd = cmd.trim_start();
    match cmd.as_bytes().first() {
        Some(b'!') => (SpawnMode::Fullscreen, cmd[1..].trim()),
        Some(b'|') => (SpawnMode::Capture, cmd[1..].trim()),
        Some(b'>') => (SpawnMode::Preview, cmd[1..].trim()),
        _ => (SpawnMode::Fullscreen, cmd.trim()),
    }
}

/// Whether `prog` can be run: an existing path, or a name found on $PATH
/// (trying %PATHEXT% extensions on Windows). Checked when a plugin runs, so
/// installing a tool mid-session just works.
pub fn which(prog: &str) -> bool {
    if prog.is_empty() {
        return false;
    }
    if prog.contains('/') || prog.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(Path::new(prog));
    }
    let Some(paths) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(prog)))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(p: &Path) -> bool {
    if p.is_file() {
        return true;
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).any(|ext| {
        let mut s = p.as_os_str().to_owned();
        s.push(ext);
        Path::new(&s).is_file()
    })
}

/// First program in `requires` that isn't installed, if any.
pub fn missing_requirement(requires: &[String]) -> Option<&str> {
    requires.iter().map(String::as_str).find(|p| !which(p))
}

/// $SHELL-ish command used to interpret plugin/open-with lines.
pub fn shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

/// Editor resolution for `e`/`E`: $VISUAL > $EDITOR > platform default.
pub fn editor() -> String {
    for v in ["VISUAL", "EDITOR"] {
        if let Ok(e) = std::env::var(v) {
            if !e.trim().is_empty() {
                return e;
            }
        }
    }
    #[cfg(unix)]
    {
        "vi".to_string()
    }
    #[cfg(windows)]
    {
        "notepad".to_string()
    }
}

/// Environment contract for plugin/previewer children.
pub fn child_env(dir: &Path, hovered: Option<&Path>, sel_file: Option<&Path>, ctx: u8) -> Vec<(String, String)> {
    let mut env = vec![
        ("VFM_DIR".to_string(), dir.to_string_lossy().into_owned()),
        ("VFM_CTX".to_string(), ctx.to_string()),
        ("VFM_VERSION".to_string(), crate::config::VERSION.to_string()),
    ];
    if let Some(p) = hovered {
        env.push(("VFM_PATH".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(f) = sel_file {
        env.push(("VFM_SELFILE".to_string(), f.to_string_lossy().into_owned()));
    }
    env
}

pub fn shell_command(cmdline: &str, cwd: &Path, env: &[(String, String)]) -> Command {
    #[cfg(unix)]
    let mut c = {
        let mut c = Command::new(shell());
        c.arg("-c").arg(cmdline);
        c
    };
    #[cfg(windows)]
    let mut c = {
        let line = cmdline.to_string();
        if line.ends_with(".ps1") {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&line);
            c
        } else {
            let mut c = Command::new(shell());
            c.arg("/C").arg(&line);
            c
        }
    };
    c.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v)));
    c
}

/// Fullscreen run: caller suspends the TUI first and restores it after; this
/// just spawns with inherited stdio and waits.
pub fn run_fullscreen(cmdline: &str, cwd: &Path, env: &[(String, String)]) -> std::io::Result<()> {
    shell_command(cmdline, cwd, env).status().map(|_| ())
}

pub struct Capture {
    /// Raw stdout (≤64KB), control lines NOT stripped.
    pub output: String,
    /// Structured replies parsed from `vfm:` lines.
    pub replies: Vec<Reply>,
    pub exit_ok: bool,
}

/// Capture run: stdout piped and bounded, stderr inherited (visible briefly
/// before the TUI resumes), stderr→null only for previews.
pub fn run_capture(cmdline: &str, cwd: &Path, env: &[(String, String)]) -> std::io::Result<Capture> {
    let out = shell_command(cmdline, cwd, env).output()?;
    let raw = &out.stdout[..out.stdout.len().min(64 * 1024)];
    let output = String::from_utf8_lossy(raw).into_owned();
    let replies = parse_replies(&output);
    Ok(Capture { output, replies, exit_ok: out.status.success() })
}

/// Control lines a plugin can emit (one per line, `vfm:` prefix). Unknown
/// `vfm:*` lines are ignored — forward compatible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Cd(PathBuf),
    Open(PathBuf),
    Refresh,
    SelectAdd(PathBuf),
    SelectRemove(PathBuf),
    SelectClear,
    Echo(String),
    Quit,
}

pub fn parse_replies(out: &str) -> Vec<Reply> {
    let mut replies = Vec::new();
    for line in out.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix(CTL_PREFIX) else { continue };
        let (word, arg) = match rest.split_once(' ') {
            Some((w, a)) => (w, a.trim()),
            None => (rest, ""),
        };
        let r = match word {
            "cd" if !arg.is_empty() => Reply::Cd(PathBuf::from(arg)),
            "open" if !arg.is_empty() => Reply::Open(PathBuf::from(arg)),
            "refresh" => Reply::Refresh,
            "select-add" if !arg.is_empty() => Reply::SelectAdd(PathBuf::from(arg)),
            "select-remove" if !arg.is_empty() => Reply::SelectRemove(PathBuf::from(arg)),
            "select-clear" => Reply::SelectClear,
            "echo" => Reply::Echo(arg.to_string()),
            "quit" => Reply::Quit,
            _ => continue,
        };
        replies.push(r);
    }
    replies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_prefixes() {
        assert_eq!(resolve_mode("| git status"), (SpawnMode::Capture, "git status"));
        assert_eq!(resolve_mode("! ranger"), (SpawnMode::Fullscreen, "ranger"));
        assert_eq!(resolve_mode("htop"), (SpawnMode::Fullscreen, "htop"));
        assert_eq!(resolve_mode("  > tree -C"), (SpawnMode::Preview, "tree -C"));
    }

    #[cfg(unix)]
    #[test]
    fn which_finds_programs_on_path() {
        assert!(which("sh"));
        assert!(which("/bin/sh"));
        assert!(!which("vfm-no-such-program-xyz"));
        assert!(!which("/nonexistent/sh"));
        assert!(!which(""));
        let req = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(missing_requirement(&req(&["sh", "vfm-nope", "x"])), Some("vfm-nope"));
        assert_eq!(missing_requirement(&req(&["sh"])), None);
        assert_eq!(missing_requirement(&[]), None);
    }

    #[test]
    fn control_lines() {
        let out = "normal output\nvfm:cd /tmp/some dir\nvfm:refresh\nvfm:unknown-thing x\nvfm:echo hello world\n\r\n";
        assert_eq!(
            parse_replies(out),
            vec![
                Reply::Cd(PathBuf::from("/tmp/some dir")),
                Reply::Refresh,
                Reply::Echo("hello world".into()),
            ]
        );
        assert!(parse_replies("vfm:cd").is_empty()); // cd without arg ignored
        assert!(parse_replies("").is_empty());
    }

    #[test]
    fn env_contract() {
        let env = child_env(Path::new("/w"), Some(Path::new("/w/f")), Some(Path::new("/sel")), 3);
        assert!(env.contains(&("VFM_DIR".into(), "/w".into())));
        assert!(env.contains(&("VFM_PATH".into(), "/w/f".into())));
        assert!(env.contains(&("VFM_SELFILE".into(), "/sel".into())));
        assert!(env.contains(&("VFM_CTX".into(), "3".into())));
        assert!(child_env(Path::new("/w"), None, None, 0).iter().all(|(k, _)| k != "VFM_PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn capture_runs_and_parses() {
        let c = run_capture(
            "printf 'out\\nvfm:cd /tmp\\n'",
            Path::new("/tmp"),
            &[],
        )
        .unwrap();
        assert!(c.exit_ok);
        assert_eq!(c.replies, vec![Reply::Cd(PathBuf::from("/tmp"))]);
        assert!(c.output.starts_with("out\n"));
    }

    #[cfg(unix)]
    #[test]
    fn fullscreen_exit_status() {
        assert!(run_fullscreen("true", Path::new("/tmp"), &[]).is_ok());
    }
}
