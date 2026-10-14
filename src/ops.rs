//! File operations on worker threads: copy, move, trash, delete. One op at a
//! time (the app holds the single OpHandle); progress flows through a bounded
//! mpsc channel and cancellation through an `Arc<AtomicBool>` — no Mutex, and
//! workers never touch screen state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Move,
    Trash,
    Delete,
}

impl OpKind {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Copy => "copying",
            Self::Move => "moving",
            Self::Trash => "trashing",
            Self::Delete => "deleting",
        }
    }
}

pub struct OpRequest {
    pub kind: OpKind,
    /// Absolute source paths.
    pub items: Vec<PathBuf>,
    /// Destination directory (Copy/Move); CopyAs passes the source's own dir.
    pub dest: Option<PathBuf>,
    /// Explicit target names, index-aligned with `items` (CopyAs).
    pub new_names: Option<Vec<String>>,
}

pub struct OpProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: String,
}

pub struct OpResult {
    pub kind: OpKind,
    pub done: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

/// Single-owner handle. `progress()` drains without blocking; `finish()`
/// joins the worker (fast — it only ever blocks on channel capacity).
pub struct OpHandle {
    rx: Receiver<OpProgress>,
    pub cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<OpResult>>,
    last: Option<OpProgress>,
}

impl OpHandle {
    pub fn progress(&mut self) -> Option<&OpProgress> {
        while let Ok(p) = self.rx.try_recv() {
            self.last = Some(p);
        }
        self.last.as_ref()
    }

    /// True once the worker returned; `finish()` will then not block.
    /// (Progress counts can't signal this: a dir rename is 1 step of N walked.)
    pub fn is_done(&self) -> bool {
        self.join.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Drain to completion and join. Called when progress shows Done — the
    /// worker always terminates after the final message.
    pub fn finish(mut self) -> OpResult {
        if let Some(h) = self.join.take() {
            h.join().unwrap_or_else(|_| OpResult {
                kind: OpKind::Copy,
                done: vec![],
                errors: vec![(PathBuf::new(), "worker panicked".into())],
                cancelled: false,
            })
        } else {
            unreachable!("finish() twice")
        }
    }
}

/// Spawn the worker thread. Always succeeds — errors surface as OpResult.
pub fn start(req: OpRequest) -> OpHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel::<OpProgress>(32);
    let cancel2 = Arc::clone(&cancel);
    let join = std::thread::Builder::new()
        .name("vfm-op".into())
        .spawn(move || run(req, tx, cancel2))
        .expect("spawn op worker");
    OpHandle { rx, cancel, join: Some(join), last: None }
}

fn run(req: OpRequest, tx: mpsc::SyncSender<OpProgress>, cancel: Arc<AtomicBool>) -> OpResult {
    let mut ctx = OpCtx {
        tx,
        cancel: &cancel,
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        errors: Vec::new(),
        cancelled: false,
    };
    // Pre-pass: totals for the progress bar (cheap; also fails fast on roots
    // that vanished). Errors here don't abort — per-file errors keep going.
    for item in &req.items {
        ctx.walk_totals(item);
        if ctx.cancel.load(Ordering::Relaxed) {
            break;
        }
    }

    let mut done = Vec::new();
    for (i, item) in req.items.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }
        // OsStr end to end: non-UTF-8 names must survive op targeting.
        let dest_item = req.dest.as_ref().map(|d| {
            let name: &std::ffi::OsStr = match &req.new_names {
                Some(n) if n.get(i).is_some() => std::ffi::OsStr::new(n[i].as_str()),
                _ => item.file_name().unwrap_or_default(),
            };
            d.join(name)
        });
        let res = match req.kind {
            OpKind::Copy => ctx.copy_item(item, dest_item.as_deref()),
            OpKind::Move => ctx.move_item(item, dest_item.as_deref()),
            OpKind::Trash => ctx.trash_item(item),
            OpKind::Delete => ctx.delete_item(item),
        };
        match res {
            Ok(()) => done.push(item.clone()),
            Err(e) => ctx.errors.push((item.clone(), e)),
        }
    }
    OpResult { kind: req.kind, done, errors: ctx.errors, cancelled: ctx.cancelled }
}

struct OpCtx<'a> {
    tx: mpsc::SyncSender<OpProgress>,
    cancel: &'a AtomicBool,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    errors: Vec<(PathBuf, String)>,
    cancelled: bool,
}

impl OpCtx<'_> {
    fn cancelled(&mut self) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            self.cancelled = true;
        }
        self.cancelled
    }

    fn walk_totals(&mut self, p: &Path) {
        match fs::symlink_metadata(p) {
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                self.files_total += 1;
                if let Ok(rd) = fs::read_dir(p) {
                    for de in rd.flatten() {
                        self.walk_totals(&de.path());
                    }
                }
            }
            Ok(md) => {
                self.files_total += 1;
                self.bytes_total += md.len();
            }
            Err(_) => {}
        }
    }

    /// One progress emission per file with the name filled in.
    fn step(&self, current: &str) {
        let _ = self.tx.try_send(OpProgress {
            files_done: self.files_done,
            files_total: self.files_total,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            current: current.to_string(),
        });
    }

    fn copy_item(&mut self, src: &Path, dest: Option<&Path>) -> Result<(), String> {
        let Some(dest) = dest else { return Err("no destination".into()) };
        check_target(src, dest)?;
        self.copy_rec(src, dest)
    }

    fn copy_rec(&mut self, src: &Path, dest: &Path) -> Result<(), String> {
        if self.cancelled() {
            return Err("cancelled".into());
        }
        let md = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
        if md.file_type().is_symlink() {
            return copy_symlink(src, dest);
        }
        if md.is_dir() {
            fs::create_dir_all(dest).map_err(|e| e.to_string())?;
            self.files_done += 1;
            self.step(&src.to_string_lossy());
            let mut errs: Option<String> = None;
            for res in fs::read_dir(src).map_err(|e| e.to_string())? {
                if self.cancelled() {
                    return Err("cancelled".into());
                }
                let Ok(de) = res else { continue };
                let child_dest = dest.join(de.file_name());
                if let Err(e) = self.copy_rec(&de.path(), &child_dest) {
                    errs = Some(errs.unwrap_or(e)); // report the first error
                }
            }
            match errs {
                Some(e) => Err(e),
                None => {
                    preserve_times(src, dest);
                    Ok(())
                }
            }
        } else {
            // Top-level collisions are refused in check_target; below it we
            // only ever write into directories this op just created.
            fs::copy(src, dest).map_err(|e| e.to_string())?;
            self.files_done += 1;
            self.bytes_done += md.len();
            self.step(&src.to_string_lossy());
            preserve_times(src, dest);
            Ok(())
        }
    }

    fn move_item(&mut self, src: &Path, dest: Option<&Path>) -> Result<(), String> {
        let Some(dest) = dest else { return Err("no destination".into()) };
        check_target(src, dest)?;
        match fs::rename(src, dest) {
            Ok(()) => {
                self.files_total = self.files_total.max(1);
                self.files_done += 1;
                self.step(&src.to_string_lossy());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                self.copy_then_remove(src, dest)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// EXDEV fallback for moves: copy, then remove the source only if the
    /// whole copy succeeded.
    fn copy_then_remove(&mut self, src: &Path, dest: &Path) -> Result<(), String> {
        self.copy_rec(src, dest)?;
        let md = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
        let rm = if md.is_dir() { fs::remove_dir_all(src) } else { fs::remove_file(src) };
        rm.map_err(|e| e.to_string())
    }

    fn trash_item(&mut self, src: &Path) -> Result<(), String> {
        #[cfg(feature = "trash")]
        {
            trash::delete(src).map_err(|e| e.to_string())?;
            self.files_done += 1;
            self.step(&src.to_string_lossy());
            Ok(())
        }
        #[cfg(not(feature = "trash"))]
        {
            let _ = src;
            Err("vfm was built without trash support (rebuild with --features trash)".into())
        }
    }

    fn delete_item(&mut self, src: &Path) -> Result<(), String> {
        let md = fs::symlink_metadata(src).map_err(|e| e.to_string())?;
        if md.is_dir() && !md.file_type().is_symlink() {
            fs::remove_dir_all(src).map_err(|e| e.to_string())?;
        } else {
            fs::remove_file(src).map_err(|e| e.to_string())?;
        }
        self.files_done += 1;
        self.step(&src.to_string_lossy());
        Ok(())
    }
}

/// Paste-target safety: never overwrite (rename/copy would clobber silently;
/// with src == dest a "replace" would delete the source), and never copy or
/// move a directory into itself (unbounded recursion).
fn check_target(src: &Path, dest: &Path) -> Result<(), String> {
    if dest.symlink_metadata().is_ok() {
        return Err("destination exists".into());
    }
    if dest.starts_with(src) {
        return Err("cannot copy/move a directory into itself".into());
    }
    Ok(())
}

fn copy_symlink(src: &Path, dest: &Path) -> Result<(), String> {
    let target = fs::read_link(src).map_err(|e| e.to_string())?;
    let res: std::io::Result<()>;
    #[cfg(unix)]
    {
        res = std::os::unix::fs::symlink(&target, dest);
    }
    #[cfg(windows)]
    {
        // metadata() follows the link; Err → orphan link, copy as file link.
        let md = fs::metadata(src);
        res = match md {
            Ok(m) if m.is_dir() => std::os::windows::fs::symlink_dir(&target, dest),
            _ => std::os::windows::fs::symlink_file(&target, dest),
        };
    }
    res.map_err(|e| e.to_string())
}

/// Best-effort mtime preservation (v1: mtime only — std has no set_accessed;
/// perms come from umask like `cp`). Dirs fail the open and keep their time.
fn preserve_times(src: &Path, dest: &Path) {
    if let Ok(md) = fs::symlink_metadata(src) {
        if let Ok(mtime) = md.modified() {
            if let Ok(f) = fs::OpenOptions::new().write(true).open(dest) {
                let _ = f.set_modified(mtime);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("sub/b.txt"), b"beta!").unwrap();
        fs::write(root.join("sub/deep/c.txt"), b"gamma").unwrap();
    }

    fn run_op(req: OpRequest) -> OpResult {
        start(req).finish() // join() blocks until the worker returns
    }

    #[test]
    fn copy_tree() {
        let d = tmpdir("copy");
        tree(&d.join("src"));
        let res = run_op(OpRequest {
            kind: OpKind::Copy,
            items: vec![d.join("src")],
            dest: Some(d.join("dst_dir")),
            new_names: None,
        });
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert_eq!(fs::read_to_string(d.join("dst_dir/src/sub/deep/c.txt")).unwrap(), "gamma");
        // Source untouched.
        assert!(d.join("src/a.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn move_into_dir() {
        let d = tmpdir("move");
        tree(&d.join("src"));
        fs::create_dir(d.join("target")).unwrap();
        let res = run_op(OpRequest {
            kind: OpKind::Move,
            items: vec![d.join("src")],
            dest: Some(d.join("target")),
            new_names: None,
        });
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        assert!(!d.join("src").exists());
        assert_eq!(fs::read_to_string(d.join("target/src/sub/b.txt")).unwrap(), "beta!");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn exdev_fallback_copies_then_removes() {
        // EXDEV can't be provoked portably in a test; drive the fallback directly.
        let d = tmpdir("exdev");
        tree(&d.join("src"));
        let (tx, _rx) = mpsc::sync_channel(32);
        let cancel = AtomicBool::new(false);
        let mut ctx = OpCtx {
            tx,
            cancel: &cancel,
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            errors: Vec::new(),
            cancelled: false,
        };
        ctx.copy_then_remove(&d.join("src"), &d.join("moved")).unwrap();
        assert!(!d.join("src").exists());
        assert_eq!(fs::read_to_string(d.join("moved/sub/deep/c.txt")).unwrap(), "gamma");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn paste_never_clobbers_or_recurses() {
        let d = tmpdir("clobber");
        fs::write(d.join("f.txt"), b"original").unwrap();
        tree(&d.join("dir"));
        // Paste into the source's own directory: dest == src must not lose data.
        for kind in [OpKind::Copy, OpKind::Move] {
            let res = run_op(OpRequest {
                kind,
                items: vec![d.join("f.txt")],
                dest: Some(d.clone()),
                new_names: None,
            });
            assert_eq!(res.errors.len(), 1, "{kind:?}");
            assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "original");
        }
        // Copy a dir into its own subtree: refused, no runaway recursion.
        let res = run_op(OpRequest {
            kind: OpKind::Copy,
            items: vec![d.join("dir")],
            dest: Some(d.join("dir/sub")),
            new_names: None,
        });
        assert_eq!(res.errors.len(), 1);
        assert!(!d.join("dir/sub/dir").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_as_with_new_name() {
        let d = tmpdir("copyas");
        fs::write(d.join("orig.txt"), b"data").unwrap();
        let res = run_op(OpRequest {
            kind: OpKind::Copy,
            items: vec![d.join("orig.txt")],
            dest: Some(d.clone()),
            new_names: Some(vec!["renamed.txt".into()]),
        });
        assert!(res.errors.is_empty());
        assert_eq!(fs::read_to_string(d.join("renamed.txt")).unwrap(), "data");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn delete_and_error_continue() {
        let d = tmpdir("del");
        fs::write(d.join("gone.txt"), b"x").unwrap();
        fs::write(d.join("stay.txt"), b"y").unwrap();
        tree(&d.join("tree"));
        let res = run_op(OpRequest {
            kind: OpKind::Delete,
            items: vec![d.join("gone.txt"), d.join("missing.txt"), d.join("tree")],
            dest: None,
            new_names: None,
        });
        // The missing item errors; the rest of the batch still runs.
        assert_eq!(res.done, vec![d.join("gone.txt"), d.join("tree")]);
        assert_eq!(res.errors.len(), 1);
        assert!(!d.join("gone.txt").exists());
        assert!(!d.join("tree").exists());
        assert!(d.join("stay.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_flag_stops_work() {
        let d = tmpdir("cancel");
        // Enough files that the worker takes more than one loop pass.
        for i in 0..2000 {
            fs::write(d.join(format!("f{i}.txt")), vec![b'x'; 1024]).unwrap();
        }
        let dest = d.join("out");
        fs::create_dir_all(&dest).unwrap();
        let h = start(OpRequest {
            kind: OpKind::Copy,
            items: (0..2000).map(|i| d.join(format!("f{i}.txt"))).collect(),
            dest: Some(dest.clone()),
            new_names: None,
        });
        h.cancel.store(true, Ordering::Relaxed);
        let res = h.finish();
        assert!(res.cancelled);
        assert!(res.done.len() < 2000);
        let _ = fs::remove_dir_all(&d);
    }
}
