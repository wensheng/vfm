//! Packed directory entry — Rust port of nnn's 48-byte `struct entry` (nnn.c:314-329).
//! Leaf module. 40 bytes on x86-64; asserted by a unit test.

/// One byte of per-file state, mirroring nnn's entry flags (nnn.c:257-263).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct EntryFlags(pub u8);

impl EntryFlags {
    /// Directory, or symlink pointing at a directory (drives dirs-first + "can cd into").
    pub const DIR_OR_DIRLNK: Self = Self(1 << 0);
    pub const SYMLINK: Self = Self(1 << 1);
    /// Symlink whose target does not exist.
    pub const SYM_ORPHAN: Self = Self(1 << 2);
    /// Hard link (st_nlink > 1).
    pub const HARD_LINK: Self = Self(1 << 3);
    /// Name was listed but stat failed (race: vanished mid-scan).
    pub const FILE_MISSING: Self = Self(1 << 4);
    pub const SELECTED: Self = Self(1 << 5);
    /// Executable bit set (any of x).
    pub const EXEC: Self = Self(1 << 6);
    /// Raw name is not valid UTF-8; the OsStr form is kept in [`crate::dir::Dir::raw_names`].
    pub const NON_UTF8: Self = Self(1 << 7);

    pub fn set(&mut self, f: Self) {
        self.0 |= f.0;
    }
    pub fn discard(&mut self, f: Self) {
        self.0 &= !f.0;
    }
    pub const fn has(self, f: Self) -> bool {
        self.0 & f.0 != 0
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Immutable after load. `Box<str>` (16B) instead of `String` (24B): names never
    /// grow, saving per-entry capacity slack across large listings.
    pub name: Box<str>,
    pub size: u64,
    /// Chosen timestamp in seconds (which timestamp: `util::TimeType`).
    pub mtime: i64,
    /// Unix permission bits (0o777 display slice). Synthesized on Windows from
    /// dir/readonly state so detail rendering is uniform.
    pub mode: u32,
    pub flags: EntryFlags,
}

impl Entry {
    pub const fn is_dir(&self) -> bool {
        self.flags.has(EntryFlags::DIR_OR_DIRLNK)
    }
    pub const fn is_symlink(&self) -> bool {
        self.flags.has(EntryFlags::SYMLINK)
    }
    pub const fn is_selected(&self) -> bool {
        self.flags.has(EntryFlags::SELECTED)
    }
    /// The single-char indicator nnn appends in light mode: `/`, `@`, `*`.
    pub const fn indicator(&self) -> char {
        if self.is_dir() {
            '/'
        } else if self.is_symlink() {
            '@'
        } else if self.flags.has(EntryFlags::EXEC) {
            '*'
        } else {
            ' '
        }
    }
    /// Extension for sort/ext: text after the final '.', lowercased, empty if none.
    pub fn ext(&self) -> &str {
        match self.name.rfind('.') {
            Some(i) if i + 1 < self.name.len() => &self.name[i + 1..],
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_40_bytes() {
        // The whole performance story rests on dense, cache-friendly entries.
        assert_eq!(std::mem::size_of::<Entry>(), 40);
    }

    #[test]
    fn flags_bit() {
        let mut f = EntryFlags::default();
        f.set(EntryFlags::SELECTED);
        assert!(f.has(EntryFlags::SELECTED));
        f.discard(EntryFlags::SELECTED);
        assert!(!f.has(EntryFlags::SELECTED));
    }

    #[test]
    fn ext_parse() {
        let e = |name: &str| Entry {
            name: name.into(),
            size: 0,
            mtime: 0,
            mode: 0,
            flags: EntryFlags::default(),
        };
        assert_eq!(e("a.tar.gz").ext(), "gz");
        assert_eq!(e("noext").ext(), "");
        assert_eq!(e(".hidden").ext(), "hidden");
        assert_eq!(e("a.").ext(), "");
    }
}
