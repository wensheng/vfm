# vfm

A small, fast terminal file manager in safe Rust.
Linux, macOS and Windows.

- **Small:** ~1.2 MB stripped binary; 5 dependencies (crossterm, unicode-width,
  toml, regex-lite, trash).
- **Fast:** one directory in memory, 40-byte entries, lazy sorting, and a
  two-line redraw for cursor moves. Measured: ~3 ms to load and render a
  2,700-entry dir, ~12 ms for 20,000 entries, 4.4 MB RSS.
- **Safe:** no `unsafe`, no `Mutex`. Workers talk to the UI over channels and
  atomics only.
- **Plugins for preview:** any command that prints to stdout (ANSI colors
  supported) can be a previewer.

## Build

```sh
cargo build --release          # target/release/vfm
cargo build --release --no-default-features   # without recycle-bin support
```

## Usage

```
vfm [-c CONFIG] [PATH]
```

Press `?` inside vfm for the full key list. The essentials:

| Keys | Action |
|---|---|
| `j` `k` / arrows, `g` `G`, `C-d` `C-u` | move |
| `l` / `Enter`, `h` | enter, parent |
| `/` | filter (`C-n` cycles substring → fuzzy → regex) |
| `1`–`8`, `Tab` | contexts (tabs) |
| `Space`, `m`, `a`, `A` | select, range, all, invert |
| `p` `v` `w` | copy / move selection here, copy as… |
| `x` / `X` | trash / delete (confirm with `y`) |
| `n`, `C-r` | new file (trailing `/` = dir), rename |
| `s`, `C-t`, `T` | cycle sort key, reverse sort, cycle time type |
| `P` | toggle preview pane (terminal ≥ 100 columns) |
| `t` | tree view of the highlighted folder in the preview pane (needs `tree`) |
| `;` | plugin menu |
| `q`, `C-q`, `C-g` | close context, quit, quit and cd |

`Esc` cancels a running file op, closes a plugin's preview, clears an active
filter, or closes an overlay.

### cd on quit

`C-g` writes the current directory to `$VFM_CD_FILE`. A shell wrapper:

```sh
vf() {
    VFM_CD_FILE="${TMPDIR:-/tmp}/vfm-cd.$$" command vfm "$@"
    [ -s "${TMPDIR:-/tmp}/vfm-cd.$$" ] && cd "$(cat "${TMPDIR:-/tmp}/vfm-cd.$$")"
    rm -f "${TMPDIR:-/tmp}/vfm-cd.$$"
}
```

## Configuration

`$VFM_CONFIG_HOME/config.toml`, else `$XDG_CONFIG_HOME/vfm/config.toml`, else
`~/.config/vfm/config.toml`. Every key is optional.

```toml
[ui]
show_hidden = false
detail      = true        # perms/date/size columns
preview     = false       # preview pane on at startup
wrap_nav    = true        # j on the last entry wraps to the first
scrolloff   = 3
time_type   = "modified"  # modified | accessed | created

[sort]
key        = "name"       # name | mtime | size | ext
reverse    = false
dirs_first = true

[filter]
mode      = "substring"   # substring | fuzzy | regex
smartcase = true

[preview]
enabled     = true
debounce_ms = 40
max_bytes   = 262144      # plugin output beyond this is dropped (marked truncated)

[colors]                  # name, 0-255, or 0xRRGGBB
dir = "blue"
symlink = "cyan"
orphan = "red"
exec = "green"
hardlink = "yellow"

[keys]                    # action = key: "Z", "C-r", "<f2>", "<esc>", ...
quit = "C-x"

[[previewer]]             # first matching rule wins
match = ["*.rs", "*.toml", "*.md"]
run   = "bat --color=always --style=plain"

[[plugin]]
name     = "git status"
cmd      = "| git status" # see "Plugin modes" below
key      = "<f2>"         # optional direct key
requires = "git"          # optional: program(s) that must be on PATH
```

Environment: `VFM_SEL` (selection file path), `VFM_FCOLORS`
(`dir=red:link=6:exec=0x88cc88`), `NO_COLOR`.

## Plugin protocol

**Previewers** run as `<run…> PATH WIDTH HEIGHT`, with stdout piped. vfm parses
SGR colors and strips other escape sequences. With no matching rule, a built-in
preview shows text heads, directory listings, symlink targets, and binary file
types. Moving the cursor kills a slow previewer immediately; recent results are
cached.

### Plugin modes

The first character of `cmd` picks how the output is shown:

| Prefix | Mode |
|---|---|
| `!` or none | fullscreen: vfm steps aside, the program owns the terminal |
| `\|` | capture: stdout shown in a pager (colors kept) |
| `>` | preview: stdout streamed into the preview pane, which opens if needed. It stays until you move the cursor, change directory, press `Esc`, or press the plugin's key again, and reruns when the directory changes or the terminal resizes. On terminals narrower than 100 columns it falls back to the pager. |

A plugin whose `requires` programs aren't on `PATH` won't run. Its key shows a
"needs `…`" message instead, and the help screen (`?`) marks it as not installed.

### Built-in plugin: tree

vfm ships one plugin, equivalent to:

```toml
[[plugin]]
name     = "tree"
cmd      = '> tree -C --dirsfirst -L 3 "${VFM_PATH:-$VFM_DIR}"'
key      = "t"
requires = "tree"
```

A `[[plugin]]` with `name = "tree"` replaces it, for example to change the depth or
the key (`key = ""` unbinds it). A key you assign in `[keys]` always takes
precedence over a plugin's key.

The tree starts at the highlighted folder. If the listing is empty, it uses the
current directory. On Windows, the built-in command is
`> if defined VFM_PATH (tree "%VFM_PATH%" /F /A) else (tree /F /A)`.

### Environment and control lines

**Plugins** run through `$SHELL -c` (`cmd /C` on Windows; `.ps1` via
PowerShell) in the current directory, with this environment:

| Variable | Value |
|---|---|
| `VFM_DIR` | current directory |
| `VFM_PATH` | hovered entry |
| `VFM_SELFILE` | NUL-separated selection file (only if something is selected) |
| `VFM_CTX` | context number (0–7) |
| `VFM_VERSION` | vfm version |

A capture-mode (`|`) plugin can control vfm by printing lines that start with `vfm:`:

```
vfm:cd <path>          vfm:select-add <path>
vfm:open <path>        vfm:select-remove <path>
vfm:refresh            vfm:select-clear
vfm:echo <text>
```

Unknown `vfm:` lines are ignored, and control lines are hidden from the pager.

## Development

```sh
cargo test                                  # 85 tests, headless
cargo test --release filter_20k             # enforces <8 ms per filter keystroke
target/release/vfm --bench /usr/lib         # load + one frame, timings on stderr
```
