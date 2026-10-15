//! Hand-rolled flag parser (no clap): `vfm [FLAGS] [PATH]`.
//! `--bench` is a hidden perf-harness flag: load PATH, render one frame, exit.

use std::path::PathBuf;

pub struct Cli {
    pub path: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub version: bool,
    pub help: bool,
    pub bench: bool,
}

pub fn usage() -> &'static str {
    "vfm - terminal file manager

USAGE:
    vfm [FLAGS] [PATH]

ARGS:
    PATH    Directory to open (default: $PWD)

FLAGS:
    -c <FILE>    Config file (default: $VFM_CONFIG_HOME/config.toml
                 or ~/.config/vfm/config.toml)
    -v           Print version and exit
    -h           Print this help and exit

Environment:
    VFM_SEL         Selection file override
    VFM_CONFIG_HOME Config/data directory override
    VFM_FCOLORS     File color overrides (type=attribute,... like NNN_FCOLORS)
    VFM_NERD_FONT   File icons: 1/true to enable, 0/false to disable
    NO_COLOR        Disable colors entirely

Keys: press ? inside vfm for the full list."
}

pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli { path: None, config: None, version: false, help: false, bench: false };
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        match a {
            "-c" | "--config" => {
                let v = args.get(i).ok_or("-c requires a file argument")?;
                i += 1;
                cli.config = Some(PathBuf::from(v));
            }
            "-v" | "--version" => cli.version = true,
            "-h" | "--help" => cli.help = true,
            "--bench" => cli.bench = true, // hidden
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(format!("unknown flag: {a}"));
            }
            _ => {
                if cli.path.is_some() {
                    return Err("only one PATH allowed".into());
                }
                cli.path = Some(PathBuf::from(a));
            }
        }
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_and_path() {
        let c = parse(&args(&["/tmp", "-c", "x.toml"])).unwrap();
        assert_eq!(c.path, Some(PathBuf::from("/tmp")));
        assert_eq!(c.config, Some(PathBuf::from("x.toml")));
        assert!(!c.bench);

        let c = parse(&args(&["--bench", "/usr/lib"])).unwrap();
        assert!(c.bench);
        assert_eq!(c.path, Some(PathBuf::from("/usr/lib")));

        let c = parse(&args(&[])).unwrap();
        assert!(c.path.is_none());

        assert!(parse(&args(&["-z"])).is_err());
        assert!(parse(&args(&["-c"])).is_err());
        assert!(parse(&args(&["a", "b"])).is_err());
        assert!(parse(&args(&["--help"])).unwrap().help);
    }
}
