//! Small utilities: size formatting (port of nnn `coolsize`), time formatting,
//! path display, glob matching and shell-word splitting. Leaf module — no deps.

use std::path::{Path, PathBuf};

/// Integer-only human size, port of nnn `coolsize()` (nnn.c:4807).
/// 1536 -> "1.5K", 1048575 -> "1023.9K". No rounding, no floats.
pub fn coolsize(size: u64) -> String {
    const UNITS: [&str; 8] = ["B", "K", "M", "G", "T", "P", "E", "Z"];
    let mut quot = size;
    let mut rem = 0u64;
    let mut i = 0usize;
    while quot >= 1024 && i < UNITS.len() - 1 {
        rem = quot % 1024;
        quot /= 1024;
        i += 1;
    }
    if i == 0 {
        format!("{quot}B")
    } else {
        format!("{quot}.{}{}", rem * 10 / 1024, UNITS[i])
    }
}

/// Which timestamp an Entry carries (nnn `-a`-style time types; std has no ctime).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum TimeType {
    #[default]
    Modified,
    Accessed,
    Created,
}

impl TimeType {
    pub fn next(self) -> Self {
        match self {
            Self::Modified => Self::Accessed,
            Self::Accessed => Self::Created,
            Self::Created => Self::Modified,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "mtime",
            Self::Accessed => "atime",
            Self::Created => "crtime",
        }
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days-to-civil conversion (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Local UTC offset in seconds at `unix` time. Parsed once from the system
/// timezone (TZif from /etc/localtime or $TZ) — keeps the dep budget at zero.
/// Falls back to UTC when nothing parseable is found (e.g. Windows without TZ).
pub fn local_offset(unix: i64) -> i32 {
    // A single static offset would be wrong across DST transitions, so store
    // the parsed transition table (or a fixed offset for TZ-less zones).
    static TABLE: std::sync::OnceLock<TzTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(TzTable::load).offset_at(unix)
}

struct TzTable {
    /// (transition time, utc offset seconds), sorted by time.
    transitions: Vec<(i64, i32)>,
    /// Offset for zones with no transition table (UTC, fixed TZ strings).
    fixed: i32,
}

impl TzTable {
    fn load() -> Self {
        if let Some(t) = Self::from_tzif(&Self::tzif_path()) {
            return t;
        }
        if let Some(tz) = std::env::var("TZ").ok().filter(|s| !s.is_empty()) {
            // POSIX fixed-offset form: "UTC-8", "EST5", "XYZ+5:30" (sign is inverted).
            if let Some(off) = Self::parse_posix_offset(&tz) {
                return Self { transitions: Vec::new(), fixed: off };
            }
            let p = Path::new("/usr/share/zoneinfo").join(tz.trim_start_matches(':'));
            if let Some(t) = Self::from_tzif(&p) {
                return t;
            }
        }
        Self { transitions: Vec::new(), fixed: 0 }
    }

    fn tzif_path() -> PathBuf {
        match std::env::var("TZ") {
            Ok(tz) if !tz.is_empty() => {
                let tz = tz.trim_start_matches(':');
                let p = PathBuf::from(tz);
                if p.is_absolute() {
                    p
                } else {
                    Path::new("/usr/share/zoneinfo").join(tz)
                }
            }
            _ => PathBuf::from("/etc/localtime"),
        }
    }

    /// "EST5", "UTC+5:30" -> offset seconds (POSIX: west-positive sign).
    fn parse_posix_offset(tz: &str) -> Option<i32> {
        let b = tz.as_bytes();
        let mut i = 0;
        while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'<') {
            i += 1;
        }
        if i == 0 || i >= b.len() {
            return None;
        }
        let (sign, rest) = match b[i] {
            b'-' => (1i32, &tz[i + 1..]),
            b'+' => (-1i32, &tz[i + 1..]),
            b'0'..=b'9' => (-1i32, &tz[i..]),
            _ => return None,
        };
        let mut parts = rest.split(':');
        let h: i32 = parts.next()?.parse().ok()?;
        let m: i32 = parts.next().map(|s| s.parse().ok()).unwrap_or(Some(0))?;
        let total = h * 3600 + m * 60;
        // "UTC-8" means UTC+8 in POSIX; a plain named zone with no digits is not this format.
        if rest.chars().all(|c| c.is_ascii_digit() || c == ':') {
            Some(sign * total)
        } else {
            None
        }
    }

    fn from_tzif(path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        // Minimal TZif reader: header, transition times + type indices, type offsets.
        // Prefers the v2+/64-bit block when present.
        let parse_block = |d: &[u8], off: &mut usize, wide: bool| -> Option<Self> {
            if d.len() < *off + 44 || &d[*off..*off + 4] != b"TZif" {
                return None;
            }
            let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            let h = &d[*off + 20..*off + 44];
            let (timecnt, typecnt) = (be32(&h[12..16]) as usize, be32(&h[16..20]) as usize);
            *off += 44;
            let tsize = if wide { 8 } else { 4 };
            if d.len() < *off + timecnt * (tsize + 1) + typecnt * 6 {
                return None;
            }
            let mut times = Vec::with_capacity(timecnt);
            for k in 0..timecnt {
                let b = &d[*off + k * tsize..];
                let t = if wide {
                    i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                } else {
                    i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64
                };
                times.push(t);
            }
            let idx_base = *off + timecnt * tsize;
            let idx: Vec<u8> = d[idx_base..idx_base + timecnt].to_vec();
            let ty_base = idx_base + timecnt;
            let offsets: Vec<i32> = (0..typecnt)
                .map(|k| i32::from_be_bytes(d[ty_base + k * 6..ty_base + k * 6 + 4].try_into().unwrap()))
                .collect();
            *off = ty_base + typecnt * 6;
            let transitions = times
                .into_iter()
                .zip(idx)
                .filter(|(_, i)| (*i as usize) < offsets.len())
                .map(|(t, i)| (t, offsets[i as usize]))
                .collect();
            Some(Self { transitions, fixed: 0 })
        };

        let mut off = 0usize;
        let v1 = parse_block(&data, &mut off, false)?;
        match data.get(off) {
            Some(b'\n') | Some(b'2'..=b'4') => {
                // v2+: repeat block with 64-bit times (skip the footer TZ string).
                if let Some(v2) = parse_block(&data, &mut off, true) {
                    return Some(v2);
                }
            }
            _ => {}
        }
        Some(v1)
    }

    fn offset_at(&self, unix: i64) -> i32 {
        if self.transitions.is_empty() {
            return self.fixed;
        }
        let i = self
            .transitions
            .partition_point(|(t, _)| *t <= unix)
            .saturating_sub(1);
        // Before the first transition, or past the last: clamp to nearest known.
        if unix < self.transitions[0].0 {
            self.transitions[0].1
        } else {
            self.transitions[i].1
        }
    }
}

/// Format a unix timestamp the way nnn's `print_time` does, in local time:
/// "Oct 14 09:30" when within the last year, else "Oct 14  2025".
pub fn fmt_time(unix: i64, now: i64) -> String {
    fmt_time_offset(unix, now, local_offset(unix))
}

/// Pure version of [`fmt_time`] with the UTC offset (seconds) given —
/// testable without a system timezone.
pub fn fmt_time_offset(unix: i64, now: i64, offset_secs: i32) -> String {
    let local = unix + offset_secs as i64;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm) = ((secs / 3600) as u32, ((secs % 3600) / 60) as u32);
    let recent = now - unix < 365 * 86_400 && now >= unix;
    if recent {
        format!("{} {:2} {:02}:{:02}", MONTHS[(m - 1) as usize], d, hh, mm)
    } else {
        format!("{} {:2} {:>5}", MONTHS[(m - 1) as usize], d, y)
    }
}

/// Render a path with $HOME abbreviated to `~`.
pub fn tilde_path(path: &Path, home: Option<&Path>) -> String {
    // Compare display forms so canonical Windows paths still abbreviate HOME.
    #[cfg(windows)]
    let (shown, shown_home) = (windows_display_path(path), home.map(windows_display_path));
    #[cfg(windows)]
    let (path, home) = (Path::new(&shown), shown_home.as_deref().map(Path::new));
    let s = path.to_string_lossy();
    match home {
        Some(h) if path.starts_with(h) && h != Path::new("/") => {
            let rest = path.strip_prefix(h).unwrap_or(path).to_string_lossy();
            if rest.is_empty() { "~".into() } else { format!("~{SEP}{rest}") }
        }
        _ => s.into(),
    }
}

/// Hide Windows' verbatim marker while retaining drive and network roots.
#[cfg(windows)]
fn windows_display_path(path: &Path) -> String {
    use std::path::{Component, Prefix};

    let text = path.to_string_lossy();
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) => text.trim_start_matches(r"\\?\").to_owned(),
            Prefix::VerbatimUNC(_, _) => format!(r"\\{}", text.trim_start_matches(r"\\?\UNC\")),
            _ => text.into_owned(),
        },
        _ => text.into_owned(),
    }
}

#[cfg(windows)]
pub const SEP: char = '\\';
#[cfg(not(windows))]
pub const SEP: char = '/';

/// Truncate `s` to `cols` display columns, ending in `…` only when it
/// actually overflows. Returns (text, width used).
pub fn truncate_cols(s: &str, cols: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let cw = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    let total: usize = s.chars().map(cw).sum();
    if total <= cols {
        return (s.to_string(), total);
    }
    if cols == 0 {
        return (String::new(), 0);
    }
    let (mut out, mut w) = (String::new(), 0usize);
    for c in s.chars() {
        if w + cw(c) > cols - 1 {
            break; // keep one cell for the ellipsis
        }
        w += cw(c);
        out.push(c);
    }
    out.push('…');
    (out, w + 1)
}

/// Classic wildcard match over a filename: `*`, `?`, literals. Case-sensitive.
pub fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    // Iterative backtracking matcher — O(p*n) worst case, no recursion.
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while n < name.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            p += 1;
            mark = n;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Split a command line the POSIX-sh way: whitespace-separated, with
/// single quotes, double quotes and backslash escapes. No variable expansion.
pub fn shlex(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let (mut cur, mut has) = (String::new(), false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            '\'' => {
                has = true;
                for c in it.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                has = true;
                while let Some(c) = it.next() {
                    match c {
                        '"' => break,
                        '\\' => match it.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                            Some(e) => {
                                cur.push('\\');
                                cur.push(e);
                            }
                            None => cur.push('\\'),
                        },
                        _ => cur.push(c),
                    }
                }
            }
            '\\' => {
                has = true;
                cur.push(it.next().unwrap_or('\\'));
            }
            _ => {
                has = true;
                cur.push(c);
            }
        }
    }
    if has {
        out.push(cur);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Current time as unix seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn tilde_path_hides_verbatim_markers_and_preserves_windows_roots() {
        for (path, expected) in [
            (r"\\?\F:\", r"F:\"),
            (r"\\?\F:\some dir", r"F:\some dir"),
            (r"\\?\UNC\server\share\folder", r"\\server\share\folder"),
            (r"\\server\share\folder", r"\\server\share\folder"),
            (r"F:\some dir", r"F:\some dir"),
        ] {
            assert_eq!(tilde_path(Path::new(path), None), expected);
        }
        let home = Path::new(r"C:\Users\person");
        assert_eq!(tilde_path(Path::new(r"\\?\C:\Users\person"), Some(home)), "~");
        assert_eq!(
            tilde_path(Path::new(r"\\?\C:\Users\person\docs"), Some(home)),
            r"~\docs"
        );
        assert_eq!(
            tilde_path(
                Path::new(r"C:\Users\person\docs"),
                Some(Path::new(r"\\?\C:\Users\person"))
            ),
            r"~\docs"
        );
        assert_eq!(
            tilde_path(Path::new(r"\\?\C:\Users\person2"), Some(home)),
            r"C:\Users\person2"
        );
    }

    #[test]
    fn coolsize_goldens() {
        assert_eq!(coolsize(0), "0B");
        assert_eq!(coolsize(1023), "1023B");
        assert_eq!(coolsize(1024), "1.0K");
        assert_eq!(coolsize(1536), "1.5K");
        assert_eq!(coolsize(1048575), "1023.9K");
        assert_eq!(coolsize(1048576), "1.0M");
        assert_eq!(coolsize(1572864), "1.5M");
        assert_eq!(coolsize(5 * 1024_u64.pow(3)), "5.0G");
    }

    #[test]
    fn time_format() {
        let t = 1_760_961_600; // 2025-10-20 12:00:00 UTC
        let now = t + 100;
        assert_eq!(fmt_time_offset(t, now, 0), "Oct 20 12:00");
        assert_eq!(fmt_time_offset(t, now, 5 * 3600), "Oct 20 17:00");
        assert_eq!(fmt_time_offset(t, now, -5 * 3600), "Oct 20 07:00");
        // Crossing midnight backwards.
        assert_eq!(fmt_time_offset(t, now, -13 * 3600), "Oct 19 23:00");
        // Older than a year: year replaces the time.
        let old = fmt_time_offset(t - 400 * 86_400, now, 0);
        assert!(old.starts_with("Sep "), "old format: {old}");
        assert!(old.ends_with("2024"), "old format: {old}");
        // The system-timezone wrapper stays consistent with the pure fn.
        assert_eq!(fmt_time(t, now), fmt_time_offset(t, now, local_offset(t)));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match(b"*.md", b"readme.md"));
        assert!(!glob_match(b"*.md", b"readme.mdx"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(glob_match(b"*", b"anything at all"));
        assert!(glob_match(b"*.tar.gz", b"pkg.tar.gz"));
        assert!(!glob_match(b"*.tar.gz", b"pkg.gz"));
        assert!(glob_match(b"Makefile", b"Makefile"));
    }

    #[test]
    fn shlex_basics() {
        let eq = |input: &str, want: &[&str]| {
            let got = shlex(input).unwrap_or_default();
            let got: Vec<&str> = got.iter().map(String::as_str).collect();
            assert_eq!(got, want, "input: {input:?}");
        };
        eq("bat --style plain", &["bat", "--style", "plain"]);
        eq(r#"sh -c "echo 'a b'""#, &["sh", "-c", "echo 'a b'"]);
        eq("it's", &["its"]);
        eq(r"a\ b", &["a b"]);
        assert_eq!(shlex("   "), None);
    }

    #[test]
    fn trunc() {
        assert_eq!(truncate_cols("hello", 10).0, "hello");
        assert_eq!(truncate_cols("hello", 3).0, "he…");
        assert_eq!(truncate_cols("你好世界", 5).0, "你好…");
        // Exact fit is not an overflow.
        assert_eq!(truncate_cols("hello", 5), ("hello".into(), 5));
        assert_eq!(truncate_cols("│", 1).0, "│");
        assert_eq!(truncate_cols("你好", 3).0, "你…");
    }
}
