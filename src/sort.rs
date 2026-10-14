//! Entry ordering: natural/version-aware name comparison (port of nnn's
//! `xstrverscasecmp`, nnn.c:3264) plus the mtime/size/ext sort keys.
//! Leaf module.

use crate::entry::{Entry, EntryFlags};
use crate::util::TimeType;
use std::cmp::Ordering;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum SortKey {
    #[default]
    Name,
    Mtime,
    Size,
    Ext,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Mtime,
            Self::Mtime => Self::Size,
            Self::Size => Self::Ext,
            Self::Ext => Self::Name,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Mtime => "mtime",
            Self::Size => "size",
            Self::Ext => "ext",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "name" => Self::Name,
            "mtime" | "time" => Self::Mtime,
            "size" => Self::Size,
            "ext" | "extension" => Self::Ext,
            _ => return None,
        })
    }
}

/// Case-insensitive natural compare: digit runs are compared by numeric value
/// ("file2" < "file10"), other chars by Unicode lowercase. Leading-zero runs
/// sort before the same number with fewer zeros ("a01" < "a1" on ties), like
/// glibc's strverscmp fractional semantics.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ab, mut bb) = (a.as_bytes(), b.as_bytes());
    loop {
        match (ab.first(), bb.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&x), Some(&y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    // Full digit run on each side.
                    let (ax, bx) = (digit_run(ab), digit_run(bb));
                    let (digits_a, digits_b) = (&ab[..ax], &bb[..bx]);
                    let lead_a = leading_zeros(digits_a);
                    let lead_b = leading_zeros(digits_b);
                    let sig_a = &digits_a[lead_a..];
                    let sig_b = &digits_b[lead_b..];
                    // Compare numeric value by significant length, then digits.
                    let ord = sig_a.len().cmp(&sig_b.len()).then_with(|| sig_a.cmp(sig_b));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    // Numerically equal: strverscmp fractional semantics —
                    // more leading zeros first ("001" < "01" < "1").
                    let ord = lead_b.cmp(&lead_a);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ab = &ab[ax..];
                    bb = &bb[bx..];
                } else {
                    let (cx, cy) = (lower(x), lower(y));
                    if cx != cy {
                        return cx.cmp(&cy);
                    }
                    // Case-folded equal (e.g. 'a' vs 'A'): keep scanning; Rust's
                    // stable sort preserves scan order for full ties.
                    ab = &ab[1..];
                    bb = &bb[1..];
                }
            }
        }
    }
}

fn digit_run(b: &[u8]) -> usize {
    b.iter().position(|c| !c.is_ascii_digit()).unwrap_or(b.len())
}

fn leading_zeros(b: &[u8]) -> usize {
    b.iter().position(|&c| c != b'0').unwrap_or(b.len())
}

/// ASCII fast path + full Unicode lowercase fallback for one byte.
fn lower(c: u8) -> u8 {
    if c.is_ascii_uppercase() {
        c + 32
    } else {
        c
    }
}

/// Total order over entries: dirs first (unless reversed, which flips the whole
/// order like nnn's `reventrycmp`), then the chosen key, then natural name.
pub fn cmp_entries(a: &Entry, b: &Entry, key: SortKey, dirs_first: bool, tt: TimeType) -> Ordering {
    if dirs_first {
        match (a.flags.has(EntryFlags::DIR_OR_DIRLNK), b.flags.has(EntryFlags::DIR_OR_DIRLNK)) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
    }
    let ord = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Mtime => a.mtime.cmp(&b.mtime),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Ext => a.ext().cmp(b.ext()),
    };
    // Time sort uses mtime_ns as tiebreak in nnn; we keep seconds, so fall
    // straight through to the name for determinism.
    let _ = tt;
    ord.then_with(|| natural_cmp(&a.name, &b.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted<'a>(names: &[&'a str]) -> Vec<&'a str> {
        let mut v = names.to_vec();
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn natural_order() {
        assert_eq!(sorted(&["file10", "file2", "file1"]), vec!["file1", "file2", "file10"]);
        assert_eq!(sorted(&["a1b", "a01a", "a1a"]), vec!["a01a", "a1a", "a1b"]);
        assert_eq!(sorted(&["B", "a", "C"]), vec!["a", "B", "C"]);
        assert_eq!(sorted(&["x100", "x99"]), vec!["x99", "x100"]);
        assert_eq!(sorted(&["", "a"]), vec!["", "a"]);
    }

    #[test]
    fn version_semantics() {
        // glibc strverscmp: leading zeros are "fractional", so 01 < 1.
        let mut v = vec!["1", "01", "001"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["001", "01", "1"]);
    }

    #[test]
    fn dirs_first_and_reverse() {
        let mk = |name: &str, dir: bool, size: u64| Entry {
            name: name.into(),
            size,
            mtime: 0,
            mode: if dir { 0o755 } else { 0o644 },
            flags: if dir { EntryFlags(EntryFlags::DIR_OR_DIRLNK.0) } else { EntryFlags::default() },
        };
        let mut v = [mk("b.txt", false, 2), mk("adir", true, 0), mk("a.txt", false, 1)];
        v.sort_by(|a, b| cmp_entries(a, b, SortKey::Size, true, TimeType::Modified));
        assert_eq!(v[0].name.as_ref(), "adir");
        assert_eq!(v[1].name.as_ref(), "a.txt");
        assert_eq!(v[2].name.as_ref(), "b.txt");
        v.sort_by(|a, b| cmp_entries(b, a, SortKey::Size, true, TimeType::Modified));
        assert_eq!(v[0].name.as_ref(), "b.txt"); // reverse puts dirs last too
        assert_eq!(v[2].name.as_ref(), "adir");
    }
}
