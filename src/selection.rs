//! Multi-selection state: insertion-ordered absolute paths plus a hash set
//! for O(1) flagging during directory loads. Mirrored to an on-disk
//! NUL-separated file (nnn's `.selection`/`$NNN_SEL` model) so plugins and
//! other vfm instances can consume it.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

pub struct Selection {
    items: Vec<PathBuf>,
    set: HashSet<PathBuf>,
    /// 'm' range-select anchor (index into the current listing).
    pub range_anchor: Option<usize>,
    /// On-disk mirror ($VFM_SEL or config dir); tests point it at a tmpdir.
    pub file: PathBuf,
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
}

impl Selection {
    pub fn new() -> Self {
        Self { items: Vec::new(), set: HashSet::new(), range_anchor: None, file: Self::sel_file() }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn contains(&self, p: &Path) -> bool {
        self.set.contains(p)
    }
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    /// Insert; true when newly added.
    pub fn add(&mut self, p: &Path) -> bool {
        if self.set.insert(p.to_path_buf()) {
            self.items.push(p.to_path_buf());
            true
        } else {
            false
        }
    }

    /// Remove; true when it was present.
    pub fn remove(&mut self, p: &Path) -> bool {
        if self.set.remove(p) {
            self.items.retain(|x| x != p);
            true
        } else {
            false
        }
    }

    /// Toggle; returns whether the path is selected after the call.
    pub fn toggle(&mut self, p: &Path) -> bool {
        if self.remove(p) {
            false
        } else {
            self.add(p);
            true
        }
    }

    /// Invert selection over the current listing (nnn `A`).
    pub fn invert(&mut self, paths: impl Iterator<Item = PathBuf>) {
        let all: Vec<PathBuf> = paths.collect();
        for p in all {
            self.toggle(&p);
        }
    }

    /// Select everything visible.
    pub fn select_all(&mut self, paths: impl Iterator<Item = PathBuf>) {
        for p in paths {
            self.add(&p);
        }
    }

    /// Drop paths that no longer exist (after move/delete ops).
    pub fn prune_missing(&mut self) {
        self.items.retain(|p| p.symlink_metadata().is_ok());
        self.set = self.items.iter().cloned().collect();
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.set.clear();
        self.range_anchor = None;
    }

    /// File the selection is persisted to ($VFM_SEL override).
    pub fn sel_file() -> PathBuf {
        if let Ok(s) = std::env::var("VFM_SEL") {
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        crate::config::config_dir().join("selection")
    }

    /// Rewrite the on-disk mirror. Called on every mutation — the file is a
    /// few KB at worst and it is the plugin interop contract, so keep it hot.
    pub fn write_sel(&self) -> std::io::Result<()> {
        self.write_to(&self.file)
    }

    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if self.items.is_empty() {
            // Empty selection: truncate the file so consumers see the change.
            std::fs::File::create(path)?;
            return Ok(());
        }
        let mut f = std::fs::File::create(path)?;
        for p in &self.items {
            f.write_all(p.as_os_str().as_encoded_bytes())?;
            f.write_all(&[0])?;
        }
        Ok(())
    }

    /// Read back a selection file (after `E` edits it in $EDITOR).
    pub fn reload_from_disk(&mut self) -> std::io::Result<usize> {
        let f = self.file.clone();
        self.reload_from(&f)
    }

    pub fn reload_from(&mut self, path: &Path) -> std::io::Result<usize> {
        let data = std::fs::read(path)?;
        let mut items = Vec::new();
        for part in data.split(|&b| b == 0) {
            if part.is_empty() {
                continue;
            }
            #[cfg(unix)]
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(part));
            #[cfg(windows)]
            let p = PathBuf::from(
                std::string::String::from_utf8_lossy(part).into_owned(), // best effort
            );
            items.push(p);
        }
        self.set = items.iter().cloned().collect();
        self.items = items;
        Ok(self.items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-sel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn toggle_and_dedup() {
        let mut s = Selection::new();
        let a = Path::new("/tmp/a");
        assert!(s.toggle(a)); // now selected
        assert!(!s.toggle(a)); // now deselected
        assert!(s.is_empty());
        s.add(a);
        assert!(!s.add(a)); // dedup
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn roundtrip_nul_file() {
        let d = tmpdir("rt");
        let f = d.join("sel");
        let mut s = Selection::new();
        s.add(Path::new("/tmp/some dir/file with space"));
        s.add(Path::new("/tmp/ünïcode"));
        s.write_to(&f).unwrap();

        let mut r = Selection::new();
        assert_eq!(r.reload_from(&f).unwrap(), 2);
        assert!(r.contains(Path::new("/tmp/ünïcode")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
