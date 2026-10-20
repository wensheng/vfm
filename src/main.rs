//! vfm — terminal file manager. Entry point: panic hook first (it restores
//! the terminal), then config load, App::run, teardown (+ $VFM_CD_FILE write
//! for `C-g`, mirroring nnn's NNN_CD_FILE shell-wrapper contract).

mod action;
mod ansi;
mod app;
mod cli;
mod config;
mod context;
mod dir;
mod entry;
mod filetype;
mod filter;
mod graphics;
mod ops;
mod plugin;
mod preview;
mod selection;
mod sort;
mod term;
mod ui;
mod util;

use app::{App, CrosstermSource, EventSource, Flow};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

// ---------------------------------------------------------------- panic hook

/// Restores the terminal if a panic unwinds while the TUI is up. Idempotent:
/// firing all four commands is harmless when none were armed.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort restore; must not panic here (panic=abort in release,
        // but a double panic in debug would lose the restore).
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stderr(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show,
            crossterm::event::DisableMouseCapture
        );
        let _ = write!(io::stderr(), "\r\n");
        default(info);
    }));
}

// ---------------------------------------------------------------- bench source

/// Never delivers events — drives the `--bench` one-shot harness.
struct NullSource;

impl EventSource for NullSource {
    fn poll(&mut self, _t: std::time::Duration) -> io::Result<bool> {
        Ok(false)
    }
    fn read(&mut self) -> io::Result<crossterm::event::Event> {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "bench"))
    }
}

fn bench(cfg: config::Config, path: Option<PathBuf>) -> ExitCode {
    let start = path.unwrap_or_else(|| PathBuf::from("."));
    let (w, h) = ui::Screen::size_of_stdout();
    let screen = ui::Screen::new(Box::new(io::sink()), w.max(80), h.max(24));
    let t0 = Instant::now();
    let mut app = match App::new(cfg, start, screen, Box::new(NullSource)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vfm --bench: {e}");
            return ExitCode::from(1);
        }
    };
    let load = t0.elapsed();
    app.render();
    let total = t0.elapsed();
    eprintln!("vfm --bench: load {load:?}, load+render {total:?}");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- main

fn main() -> ExitCode {
    install_panic_hook();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vfm: {e}\n\n{}", cli::usage());
            return ExitCode::from(2);
        }
    };
    if parsed.help {
        print!("{}", cli::usage());
        return ExitCode::SUCCESS;
    }
    if parsed.version {
        println!("vfm {}", config::VERSION);
        return ExitCode::SUCCESS;
    }

    let (mut cfg, cfg_err) = config::Config::load(parsed.config.as_deref());
    config::apply_env_overrides(&mut cfg);

    if parsed.bench {
        return bench(cfg, parsed.path);
    }

    let start = parsed.path.unwrap_or_else(|| PathBuf::from("."));
    let (w, h) = ui::Screen::size_of_stdout();
    let mut screen = ui::Screen::new(Box::new(io::stdout()), w.max(20), h.max(6));
    if let Err(e) = screen.enter() {
        eprintln!("vfm: terminal setup failed: {e}");
        return ExitCode::from(1);
    }

    let mut app = match App::new(cfg, start, screen, Box::new(CrosstermSource)) {
        Ok(a) => a,
        Err(e) => {
            let mut s = ui::Screen::new(Box::new(io::stdout()), 80, 24);
            s.leave();
            eprintln!("vfm: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(err) = cfg_err {
        app.notify(err);
    }
    // Probe once startup can no longer fail (late replies would land in the
    // shell) and before run() paints the first frame over any query echo.
    let (kind, probe) = term::detect();
    app.set_term(kind, probe);

    let flow = app.run();

    // Teardown before anything user-visible: screen owns the tty stream.
    app.screen.leave();

    // C-g contract: hand the final directory to the calling shell wrapper.
    if let (Some(dir), Ok(cd_file)) = (&app.quit_cd, std::env::var("VFM_CD_FILE")) {
        if !cd_file.is_empty() {
            match std::fs::File::create(&cd_file) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", dir.display());
                }
                Err(e) => eprintln!("vfm: cd file: {e}"),
            }
        }
    }

    match flow {
        Flow::Quit(code) => ExitCode::from(code.min(255) as u8),
        _ => ExitCode::SUCCESS,
    }
}
