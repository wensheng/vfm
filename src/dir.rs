//! The directory listing: load, lazy sort, nnn-style filter compaction, and
//! stat-poll change detection. Replaces nnn's globals (pdents/ndents/cur/
//! curscroll) with one owned struct. Exactly one Dir is alive at a time.
//!
//! Scroll is derived, not stored state: the renderer calls `scroll_adjust`
//! each frame — a single source of truth (nnn keeps curscroll as a global;
//! deriving it removes a whole class of desync bugs at equal cost).

use crate::entry::{Entry, EntryFlags};
use crate::filter::Matcher;
use crate::sort::{cmp_entries, SortKey};
use crate::util::TimeType;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const SCROLLOFF: usize = 3;

pub struct Dir {
    /// Absolute; canonicalized on cd.
    pub path: PathBuf,
    /// ALL entries. When filtered, the matching prefix [0..matches] holds the
    /// visible set; the tail keeps everything else so unfiltering is O(1)
    /// (nnn's `fill()` semantics, nnn.c:3864).
    pub entries: Vec<Entry>,
    /// Visible prefix length; == entries.len() when no filter is active.
    pub matches: usize,
    /// Cursor (nnn `cur`), index into `entries`.
    pub cursor: usize,
    /// First visible row (nnn `curscroll`); derived by scroll_adjust each frame.
    pub scroll: usize,
    pub sort: SortKey,
    pub reverse: bool,
    pub dirs_first: bool,
    pub time_type: TimeType,
    pub show_hidden: bool,
    /// Bumped whenever entries change; sorting is skipped while the stamp
    /// matches (yazi's lazy-sort revision pattern).
    sort_rev: u64,
    sorted_at_rev: u64,
    /// (mtime, len) of the directory itself, for the 1 Hz change poll.
    pub sig: Option<(SystemTime, u64)>,
    /// OsStr names for NON_UTF8 entries, index-aligned with `entries`.
    /// Only allocated when a non-UTF-8 name exists — almost always None.
    pub raw_names: Option<Vec<Box<OsStr>>>,
}

impl Dir {
    /// Read a directory. Symlinks get one extra follow-stat (orphan/dir-link
    /// detection) — every other entry costs exactly one `DirEntry::metadata`,
    /// which does NOT follow symlinks (matches nnn's dentfill syscall budget).
    /// `show_hidden = false` skips dot-entries at load time (nnn fills them
    /// in only on toggle; skipping keeps `raw_names` alignment trivial).
    pub fn load(path: &Path, tt: TimeType, show_hidden: bool) -> std::io::Result<Dir> {
        let mut entries: Vec<Entry> = Vec::with_capacity(256);
        let mut raw_names: Option<Vec<Box<OsStr>>> = None;

        for res in fs::read_dir(path)? {
            let Ok(de) = res else { continue };
            let os_name = de.file_name();
            if !show_hidden && os_name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            let (name, non_utf8) = match os_name.to_str() {
                Some(s) => (Box::from(s), false),
                None => (os_name.to_string_lossy().into_owned().into_boxed_str(), true),
            };
            let mut flags = EntryFlags::default();
            if non_utf8 {
                flags.set(EntryFlags::NON_UTF8);
            }

            let (size, mtime, mode, extra_flags) = match de.metadata() {
                Ok(meta) => {
                    let ft = meta.file_type();
                    let mut f = EntryFlags::default();
                    let exec;
                    let mode;
                    if ft.is_symlink() {
                        f.set(EntryFlags::SYMLINK);
                        match fs::metadata(path.join(&os_name)) {
                            Ok(target) => {
                                if target.is_dir() {
                                    f.set(EntryFlags::DIR_OR_DIRLNK);
                                }
                                if nlink(&target) > 1 {
                                    f.set(EntryFlags::HARD_LINK);
                                }
                            }
                            Err(_) => {
                                f.set(EntryFlags::SYM_ORPHAN);
                            }
                        }
                    } else if ft.is_dir() {
                        f.set(EntryFlags::DIR_OR_DIRLNK);
                    } else if nlink(&meta) > 1 {
                        f.set(EntryFlags::HARD_LINK);
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        mode = meta.permissions().mode() & 0o777;
                        exec = !ft.is_dir() && mode & 0o111 != 0;
                    }
                    #[cfg(windows)]
                    {
                        mode = if ft.is_dir() {
                            0o755
                        } else if meta.permissions().readonly() {
                            0o444
                        } else {
                            0o644
                        };
                        exec = !ft.is_dir()
                            && os_name.to_str().is_some_and(|n| {
                                let e = n.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                                matches!(e.as_str(), "exe" | "bat" | "cmd" | "com")
                            });
                    }
                    if exec {
                        f.set(EntryFlags::EXEC);
                    }
                    let mtime = match tt {
                        TimeType::Modified => meta.modified(),
                        TimeType::Accessed => meta.accessed(),
                        TimeType::Created => meta.created(),
                    }
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                    (if ft.is_dir() { 0 } else { meta.len() }, mtime, mode, f)
                }
                // Name vanished between readdir and stat: keep with flags only.
                Err(_) => (0, 0, 0, EntryFlags(EntryFlags::FILE_MISSING.0)),
            };
            flags.set(extra_flags);

            push_entry(
                &mut entries,
                &mut raw_names,
                &os_name,
                Entry { name, size, mtime, mode, flags },
            );
        }

        Ok(Dir {
            path: path.to_path_buf(),
            matches: entries.len(),
            entries,
            cursor: 0,
            scroll: 0,
            sort: SortKey::Name,
            reverse: false,
            dirs_first: true,
            time_type: tt,
            show_hidden,
            sort_rev: 1,
            sorted_at_rev: 0,
            sig: dir_sig(path),
            raw_names,
        })
    }

    pub fn changed(&self) -> bool {
        match self.sig {
            Some(_) => dir_sig(&self.path) != self.sig,
            None => false, // unreadable dir: don't spin on repeated failures
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Number of visible (filtered-in) entries.
    pub fn nvisible(&self) -> usize {
        self.matches
    }

    pub fn sort_dirty(&self) -> bool {
        self.sort_rev != self.sorted_at_rev
    }

    /// Mark entries as changed (forces re-sort on next ensure_sorted).
    pub fn bump(&mut self) {
        self.sort_rev += 1;
    }

    /// Sort the visible prefix if dirty. Called by render/descend paths —
    /// cheap no-op when already sorted.
    pub fn ensure_sorted(&mut self) {
        if !self.sort_dirty() {
            return;
        }
        self.sorted_at_rev = self.sort_rev;
        let (key, df, tt) = (self.sort, self.dirs_first, self.time_type);
        let n = self.matches;
        if self.reverse {
            self.entries[..n].sort_by(|a, b| cmp_entries(b, a, key, df, tt));
        } else {
            self.entries[..n].sort_by(|a, b| cmp_entries(a, b, key, df, tt));
        }
    }

    /// nnn `matches()`+`fill()`: compact matching entries to the visible
    /// prefix, (fuzzy-)sort it, and keep the cursor on the previously hovered
    /// entry when it still matches.
    pub fn apply_filter(&mut self, m: Option<&Matcher>, fuzzy: bool, prev_name: Option<&str>) {
        // Compaction scrambles the tail, so the visible prefix is always
        // re-sorted afterwards (nnn: ENTSORT after fill()). The matching
        // prefix stays in order, making that sort an O(n) adaptive pass.
        self.matches = match m {
            None => self.entries.len(),
            Some(m) => crate::filter::compact(&mut self.entries, m),
        };
        self.bump();
        self.ensure_sorted();
        if let (Some(m), true) = (m, fuzzy) {
            crate::filter::sort_fuzzy(&mut self.entries, self.matches, m);
        }
        match prev_name {
            Some(prev) => self.move_to_name(prev),
            None => self.cursor = self.cursor.min(self.matches.saturating_sub(1)),
        }
    }

    /// Re-read from disk (dir-change poll or post-op refresh), preserving
    /// sort state and the hovered name.
    pub fn repopulate(&mut self) -> std::io::Result<()> {
        let hovered = self.hovered_name().map(str::to_string);
        let (sort, reverse, dirs_first, tt, hidden) =
            (self.sort, self.reverse, self.dirs_first, self.time_type, self.show_hidden);
        let mut fresh = Dir::load(&self.path, tt, hidden)?;
        fresh.sort = sort;
        fresh.reverse = reverse;
        fresh.dirs_first = dirs_first;
        std::mem::swap(self, &mut fresh);
        self.ensure_sorted();
        if let Some(h) = hovered {
            self.move_to_name(&h);
        }
        Ok(())
    }

    pub fn hovered_name(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(|e| e.name.as_ref())
    }

    /// Path of entry `i`, using the raw OsStr form for non-UTF-8 names.
    /// Ops must always go through this (or `hovered_path`) — never rebuild
    /// paths from the lossy display string.
    pub fn path_at(&self, i: usize) -> Option<PathBuf> {
        let e = self.entries.get(i)?;
        if e.flags.has(EntryFlags::NON_UTF8) {
            if let Some(raw) = self.raw_names.as_ref().and_then(|r| r.get(i)) {
                return Some(self.path.join(raw.as_ref()));
            }
        }
        Some(self.path.join(e.name.as_ref()))
    }

    pub fn hovered_path(&self) -> Option<PathBuf> {
        self.path_at(self.cursor)
    }

    /// Move the cursor within the visible prefix, wrapping at the ends
    /// (nnn rollover). Scroll is derived at render time.
    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.matches;
        if n == 0 {
            return;
        }
        let cur = self.cursor as isize;
        self.cursor = if cur + delta < 0 {
            if cur == 0 {
                n as isize - 1
            } else {
                0
            }
        } else if cur + delta >= n as isize {
            if cur == n as isize - 1 {
                0
            } else {
                n as isize - 1
            }
        } else {
            cur + delta
        } as usize;
    }

    /// Derive `scroll` so the cursor sits SCROLLOFF rows from the pane edge
    /// (nnn's boa-constrictor squeeze, nnn.c:7660-7694). Called every frame.
    pub fn scroll_adjust(&mut self, rows: usize, scrolloff: usize) -> usize {
        if rows == 0 || self.matches == 0 {
            return 0;
        }
        let rows = rows.min(self.matches);
        let off = scrolloff.min(rows / 2);
        let mut scroll = self.scroll.min(self.matches - rows); // clamp to range
        if self.cursor < scroll + off {
            scroll = self.cursor.saturating_sub(off);
        }
        if self.cursor + off >= scroll + rows {
            scroll = (self.cursor + off + 1).saturating_sub(rows).min(self.matches - rows);
        }
        self.scroll = scroll;
        scroll
    }

    /// Cursor position restore after filtering/reload.
    pub fn move_to_name(&mut self, name: &str) {
        // Visible prefix only: the filtered-out tail must never get the cursor.
        self.cursor = self.entries[..self.matches]
            .iter()
            .position(|e| e.name.as_ref() == name)
            .unwrap_or_else(|| self.cursor.min(self.matches.saturating_sub(1)));
    }

    /// nnn `'`: jump to the first non-dir entry, else the last entry.
    pub fn jump_first_file(&mut self) {
        self.cursor = self.entries[..self.matches]
            .iter()
            .position(|e| !e.is_dir())
            .unwrap_or(self.matches.saturating_sub(1));
    }
}

/// Hard-link count where the platform exposes it (unix). Windows: 1 (skip).
#[cfg(unix)]
fn nlink(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.nlink()
}
#[cfg(windows)]
fn nlink(_m: &fs::Metadata) -> u64 {
    1
}

/// Store an entry; keep the OsStr name when any non-UTF-8 name has been seen
/// (index-aligned with `entries`; UTF-8 entries get an unused placeholder).
fn push_entry(
    entries: &mut Vec<Entry>,
    raw_names: &mut Option<Vec<Box<OsStr>>>,
    os_name: &OsStr,
    e: Entry,
) {
    if e.flags.has(EntryFlags::NON_UTF8) {
        raw_names.get_or_insert_with(Vec::new);
    }
    if let Some(raw) = raw_names.as_mut() {
        while raw.len() < entries.len() {
            raw.push(OsStr::new("").to_owned().into_boxed_os_str());
        }
        raw.push(os_name.to_owned().into_boxed_os_str());
    }
    entries.push(e);
}

pub fn dir_sig(path: &Path) -> Option<(SystemTime, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FilterMode;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn load_sorted(d: &Path) -> Dir {
        let mut dir = Dir::load(d, TimeType::Modified, true).unwrap();
        dir.ensure_sorted();
        dir
    }

    #[test]
    fn load_and_sort() {
        let d = tmpdir("sort");
        fs::create_dir(d.join("sub")).unwrap();
        // Pin the mode — the ambient umask varies.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(d.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(d.join("file10"), b"").unwrap();
        fs::write(d.join("file2"), b"xy").unwrap();

        let dir = load_sorted(&d);
        let names: Vec<&str> = dir.entries.iter().map(|e| e.name.as_ref()).collect();
        assert_eq!(names, vec!["sub", "file2", "file10"]); // dirs first, natural order
        assert_eq!(dir.entries[0].mode & 0o777, 0o755);
        assert!(dir.entries[0].is_dir());
        assert_eq!(dir.entries[1].size, 2);
        assert_eq!(dir.nvisible(), 3);
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)] // symlink creation needs privileges on Windows
    #[test]
    fn symlink_orphan_and_dirlink() {
        let d = tmpdir("links");
        fs::create_dir(d.join("real")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real", d.join("good")).unwrap();
            std::os::unix::fs::symlink("nowhere", d.join("dead")).unwrap();
        }
        let dir = load_sorted(&d);
        #[cfg(unix)]
        {
            let find = |n: &str| dir.entries.iter().find(|e| e.name.as_ref() == n).unwrap();
            let good = find("good");
            let dead = find("dead");
            assert!(good.is_symlink() && good.is_dir());
            assert!(dead.is_symlink() && dead.flags.has(EntryFlags::SYM_ORPHAN));
            assert!(!dead.is_dir());
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn filter_compaction_restores() {
        let d = tmpdir("filter");
        for n in ["alpha", "beta", "alphabet", "gamma"] {
            fs::write(d.join(n), b"").unwrap();
        }
        let mut dir = load_sorted(&d);
        let total = dir.len();

        let m = Matcher::new("alpha", FilterMode::Substring, true).unwrap();
        let prev = dir.hovered_name().map(str::to_string);
        dir.apply_filter(Some(&m), false, prev.as_deref());
        assert_eq!(dir.nvisible(), 2);
        assert_eq!(dir.len(), total); // tail preserved
        dir.apply_filter(None, false, None);
        assert_eq!(dir.nvisible(), total);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cursor_wraps_and_scroll_squeezes() {
        let d = tmpdir("cursor");
        for i in 0..20 {
            fs::write(d.join(format!("f{i:02}")), b"").unwrap();
        }
        let mut dir = load_sorted(&d);

        dir.move_cursor(1);
        assert_eq!(dir.cursor, 1);
        dir.move_cursor(-2); // clamps at 0 (wrap only happens at the edge)
        assert_eq!(dir.cursor, 0);
        dir.move_cursor(-1); // rollover to end
        assert_eq!(dir.cursor, 19);
        dir.move_cursor(1); // rollover to 0
        assert_eq!(dir.cursor, 0);

        // Scrolling: the cursor parks SCROLLOFF(3) rows above the pane bottom
        // while the list scrolls underneath (nnn's behavior).
        for _ in 0..5 {
            dir.move_cursor(1);
        }
        assert_eq!(dir.scroll_adjust(10, SCROLLOFF), 0);
        assert_eq!(dir.cursor, 5);
        for _ in 0..5 {
            dir.move_cursor(1);
        }
        assert_eq!(dir.cursor, 10);
        assert_eq!(dir.scroll_adjust(10, SCROLLOFF), 4); // cursor on pane row 6: 3 rows below
        for _ in 0..10 {
            dir.move_cursor(1); // wraps past the end back to 0
        }
        assert_eq!(dir.cursor, 0);
        assert_eq!(dir.scroll_adjust(10, SCROLLOFF), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sig_detects_change() {
        let d = tmpdir("sig");
        let dir = Dir::load(&d, TimeType::Modified, true).unwrap();
        assert!(!dir.changed());
        fs::write(d.join("new"), b"").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(dir.changed());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn repopulate_preserves_hover() {
        let d = tmpdir("repop");
        for n in ["a", "b", "c"] {
            fs::write(d.join(n), b"").unwrap();
        }
        let mut dir = load_sorted(&d);
        dir.cursor = 2; // "c"
        fs::write(d.join("d"), b"").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        dir.repopulate().unwrap();
        assert_eq!(dir.len(), 4);
        assert_eq!(dir.hovered_name(), Some("c"));
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_survive_ops_paths() {
        use std::os::unix::ffi::OsStrExt;

        let d = tmpdir("raw-paths");
        let mut dir = Dir::load(&d, TimeType::Modified, true).unwrap();
        // Build readdir records in memory: macOS filesystems reject these names.
        // Valid names around raw names also exercise raw_names index alignment.
        let names = [
            OsStr::new("before"),
            OsStr::from_bytes(b"bad\xff name"),
            OsStr::new("after"),
            OsStr::from_bytes(b"bad\xfe name"),
        ];
        for name in names {
            let flags = if name.to_str().is_none() {
                EntryFlags::NON_UTF8
            } else {
                EntryFlags::default()
            };
            push_entry(
                &mut dir.entries,
                &mut dir.raw_names,
                name,
                Entry {
                    name: name.to_string_lossy().into_owned().into_boxed_str(),
                    size: 1,
                    mtime: 0,
                    mode: 0o644,
                    flags,
                },
            );
        }
        dir.matches = dir.len();
        // Both raw names display identically, but operations must distinguish them.
        assert_eq!(dir.entries[1].name, dir.entries[3].name);
        assert!(dir.entries[1].name.contains('\u{FFFD}'));
        for (i, name) in names.iter().enumerate() {
            assert_eq!(dir.path_at(i).unwrap(), d.join(name));
            dir.cursor = i;
            assert_eq!(dir.hovered_path().unwrap(), d.join(name));
        }
        assert_eq!(dir.path_at(names.len()), None);
        fs::remove_dir_all(d).unwrap();
    }

    // macOS filesystems require valid UTF-8; raw-name path handling is tested above.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn non_utf8_names_load_from_filesystem() {
        use std::os::unix::ffi::OsStrExt;

        let d = tmpdir("raw");
        let bad = OsStr::from_bytes(b"bad\xff name");
        fs::write(d.join(bad), b"x").unwrap();
        let dir = Dir::load(&d, TimeType::Modified, true).unwrap();
        assert_eq!(dir.len(), 1);
        let e = &dir.entries[0];
        assert!(e.flags.has(EntryFlags::NON_UTF8));
        // The lossy display string differs from the raw name...
        assert!(e.name.contains('\u{FFFD}'));
        // ...but path_at returns the true bytes.
        let p = dir.path_at(0).unwrap();
        assert_eq!(p.file_name().unwrap().as_bytes(), b"bad\xff name");
        let _ = fs::remove_dir_all(&d);
    }
}
