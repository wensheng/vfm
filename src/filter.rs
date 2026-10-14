//! Filter modes ported from nnn: smartcase substring, fuzzy, regex — with
//! nnn's compaction semantics (matching entries compact to the front; the
//! original count is recoverable, so backspace "unfilters" for free).
//! Leaf module.

use crate::entry::Entry;
use regex_lite::Regex;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum FilterMode {
    #[default]
    Substring,
    Fuzzy,
    Regex,
}

impl FilterMode {
    pub fn next(self) -> Self {
        match self {
            Self::Substring => Self::Fuzzy,
            Self::Fuzzy => Self::Regex,
            Self::Regex => Self::Substring,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Substring => "str",
            Self::Fuzzy => "fzy",
            Self::Regex => "rgx",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "substring" | "str" => Self::Substring,
            "fuzzy" | "fzy" => Self::Fuzzy,
            "regex" | "rgx" => Self::Regex,
            _ => return None,
        })
    }
}

/// nnn smartcase: an uppercase char anywhere in the query makes it case-sensitive.
fn case_sensitive(needle: &str) -> bool {
    needle.chars().any(char::is_uppercase)
}

#[derive(Clone, Debug)]
pub enum Matcher {
    Substring { needle: String, case_sensitive: bool },
    Fuzzy { needle: String, case_sensitive: bool },
    Regex(Regex),
}

impl Matcher {
    pub fn new(text: &str, mode: FilterMode, smartcase: bool) -> Result<Matcher, String> {
        match mode {
            FilterMode::Regex => Regex::new(text).map(Matcher::Regex).map_err(|e| e.to_string()),
            FilterMode::Substring => Ok(Matcher::Substring {
                needle: text.to_string(),
                case_sensitive: smartcase && case_sensitive(text),
            }),
            FilterMode::Fuzzy => Ok(Matcher::Fuzzy {
                needle: text.to_string(),
                case_sensitive: smartcase && case_sensitive(text),
            }),
        }
    }

    pub fn matches(&self, e: &Entry) -> bool {
        match self {
            Matcher::Substring { needle, case_sensitive } => {
                contains_ci(&e.name, needle, *case_sensitive)
            }
            Matcher::Fuzzy { .. } => self.fuzzy_score(e).is_some(),
            Matcher::Regex(re) => re.is_match(&e.name),
        }
    }

    /// Small deterministic fuzzy scorer: all pattern chars must appear in order.
    /// Bonus for word-boundary and consecutive matches. Higher is better.
    pub fn fuzzy_score(&self, e: &Entry) -> Option<u32> {
        let Matcher::Fuzzy { needle, case_sensitive } = self else {
            return None;
        };
        if needle.is_empty() {
            return Some(0);
        }
        // Fast path (per entry, per keystroke): ASCII on both sides folds
        // without allocation. Non-ASCII needles take the general path.
        let ascii_fold = !*case_sensitive && needle.is_ascii() && e.name.is_ascii();
        let pat: Vec<char> = if ascii_fold {
            needle.chars().map(|c| c.to_ascii_lowercase()).collect()
        } else {
            needle.to_lowercase().chars().collect()
        };
        let mut score = 0u32;
        let mut pi = 0usize;
        let mut prev_hit = false;
        let mut prev: Option<char> = None;
        for c0 in e.name.chars() {
            let c = if ascii_fold { c0.to_ascii_lowercase() } else { c0 };
            if pi < pat.len() && char_eq_ci(c, pat[pi], ascii_fold) {
                score += 1;
                if prev.is_none_or(|p| !p.is_alphanumeric()) {
                    score += 2; // word boundary
                } else if prev_hit {
                    score += 1; // consecutive run
                }
                prev_hit = true;
                pi += 1;
            } else {
                prev_hit = false;
            }
            prev = Some(c);
        }
        (pi == pat.len()).then_some(score)
    }
}

/// Case-insensitive char equality; `ascii_fold` means both sides are ASCII.
fn char_eq_ci(a: char, b: char, ascii_fold: bool) -> bool {
    if ascii_fold {
        a == b // both pre-folded
    } else {
        a == b || a.to_lowercase().eq(b.to_lowercase())
    }
}

/// `hay.contains(needle)` with smartcase handling. ASCII needles fold per byte
/// with zero allocation; non-ASCII falls back to lowercase conversion.
fn contains_ci(hay: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        return hay.contains(needle);
    }
    if needle.is_ascii() {
        let nb = needle.as_bytes();
        if nb.is_empty() {
            return true;
        }
        let hb = hay.as_bytes();
        if nb.len() > hb.len() {
            return false;
        }
        let first = nb[0].to_ascii_lowercase();
        (0..=hb.len() - nb.len()).any(|i| {
            hb[i].to_ascii_lowercase() == first
                && hb[i..i + nb.len()]
                    .iter()
                    .zip(nb)
                    .all(|(h, n)| h.to_ascii_lowercase() == *n)
        })
    } else {
        hay.to_lowercase().contains(&needle.to_lowercase())
    }
}

/// Apply nnn's `fill()` compaction: matching entries are swapped to the front
/// and `Dir::matches` shrinks; the tail keeps everything else so clearing the
/// filter restores the full listing without re-reading. Returns the match count.
pub fn compact(entries: &mut [Entry], m: &Matcher) -> usize {
    let mut count = 0usize;
    for i in 0..entries.len() {
        if m.matches(&entries[i]) {
            if i != count {
                entries.swap(i, count);
            }
            count += 1;
        }
    }
    count
}

/// Sort the visible prefix by fuzzy score, best first (nnn's `fuzzyentrycmp`).
/// Applied as an in-place permutation — no per-entry allocation per keystroke.
pub fn sort_fuzzy(entries: &mut [Entry], count: usize, m: &Matcher) {
    let scores: Vec<Option<u32>> = entries[..count].iter().map(|e| m.fuzzy_score(e)).collect();
    // order[slot] = source index, best first.
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| {
        scores[b].cmp(&scores[a])
            .then_with(|| crate::sort::natural_cmp(&entries[a].name, &entries[b].name))
    });
    // Invert into dst[src] = target slot, then apply via cycle-following swaps:
    // each swap seats at least one element at its final position.
    let mut dst: Vec<usize> = vec![0; count];
    for (slot, &src) in order.iter().enumerate() {
        dst[src] = slot;
    }
    for i in 0..count {
        while dst[i] != i {
            let j = dst[i];
            entries.swap(i, j);
            dst.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryFlags;

    fn e(name: &str) -> Entry {
        Entry { name: name.into(), size: 0, mtime: 0, mode: 0o644, flags: EntryFlags::default() }
    }

    fn mk(text: &str, mode: FilterMode) -> Matcher {
        Matcher::new(text, mode, true).unwrap()
    }

    #[test]
    fn smartcase() {
        assert!(mk("foo", FilterMode::Substring).matches(&e("FOObar.txt")));
        assert!(mk("Foo", FilterMode::Substring).matches(&e("FooBar.txt")));
        assert!(!mk("Foo", FilterMode::Substring).matches(&e("foobar.txt")));
        assert!(!mk("foo", FilterMode::Substring).matches(&e("fob.txt")));
    }

    #[test]
    fn fuzzy_prefers_boundaries() {
        let m = mk("fb", FilterMode::Fuzzy);
        assert!(m.matches(&e("foo-bar")));
        assert!(m.matches(&e("foobar")));
        assert!(!m.matches(&e("nope")));
        // scorer itself
        let hi = m.fuzzy_score(&e("foo-bar")).unwrap();
        let lo = m.fuzzy_score(&e("foobar")).unwrap();
        assert!(hi > lo, "{hi} vs {lo}");
    }

    #[test]
    fn regex_mode() {
        let m = Matcher::new("^a.c$", FilterMode::Regex, true).unwrap();
        assert!(m.matches(&e("abc")));
        assert!(!m.matches(&e("xabc")));
        assert!(Matcher::new("([unclosed", FilterMode::Regex, true).is_err());
    }

    #[test]
    fn compaction_and_restore() {
        let mut v = vec![e("foo"), e("bar"), e("bazfoo"), e("qux")];
        let n = compact(&mut v, &mk("foo", FilterMode::Substring));
        assert_eq!(n, 2);
        assert_eq!(&v[..n].iter().map(|e| e.name.as_ref()).collect::<Vec<_>>(), &["foo", "bazfoo"]);
        // The tail still holds the non-matches — unfiltering is O(1).
        assert_eq!(v.len(), 4);
        assert_eq!(&v[n..].iter().map(|e| e.name.as_ref()).collect::<Vec<_>>(), &["bar", "qux"]);
    }

    #[test]
    fn fuzzy_sort_order() {
        // "foo-bar" and "f-b" tie on score (both hit the boundary); both must
        // beat the mid-word-only match "foobar". Ties fall back to name order.
        let mut v = vec![e("foobar"), e("foo-bar"), e("nope"), e("f-b")];
        let m = mk("fb", FilterMode::Fuzzy);
        let n = compact(&mut v, &m);
        assert_eq!(n, 3);
        sort_fuzzy(&mut v, n, &m);
        let names: Vec<&str> = v[..n].iter().map(|e| e.name.as_ref()).collect();
        assert_eq!(names.last(), Some(&"foobar")); // worst match last
        assert!(m.fuzzy_score(&e("foo-bar")).unwrap() > m.fuzzy_score(&e("foobar")).unwrap());
    }
}
