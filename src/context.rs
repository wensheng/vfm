//! Contexts (nnn's tabs): 8 slots, each holding strings + view settings only.
//! One directory listing is alive at a time — switching contexts always
//! repopulates from disk (nnn's model, and it's what keeps memory tiny).

use crate::filter::{FilterMode, Matcher};
use crate::sort::SortKey;
use std::path::PathBuf;

pub const CTX_MAX: usize = 8;

#[derive(Clone, Debug)]
pub struct Context {
    pub path: PathBuf,
    /// '-' target: previous directory in this context.
    pub last_dir: Option<PathBuf>,
    /// Cursor restore name when re-entering this context.
    pub last_name: Option<Box<str>>,
    /// Active filter snapshot ('/' accepts the filter; Esc clears it).
    pub filter_text: Option<Box<str>>,
    pub filter_mode: FilterMode,
    pub show_hidden: bool,
    pub sort: SortKey,
    pub reverse: bool,
}

impl Context {
    pub fn new(path: PathBuf, cfg: &crate::config::Config) -> Self {
        Self {
            path,
            last_dir: None,
            last_name: None,
            filter_text: None,
            filter_mode: cfg.filter.mode,
            show_hidden: cfg.ui.show_hidden,
            sort: cfg.sort.key,
            reverse: cfg.sort.reverse,
        }
    }

    /// Rebuild the matcher from this context's snapshot (None = no filter).
    pub fn matcher(&self) -> Option<Matcher> {
        let text = self.filter_text.as_deref()?;
        Matcher::new(text, self.filter_mode, true).ok()
    }
}
