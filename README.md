# vfm

A small, fast terminal file manager in safe Rust.
Linux, macOS and Windows.

- **Small:** ~1.2 MB stripped binary; 6 dependencies (crossterm, unicode-width,
  toml, regex-lite, trash, and libc on Unix).
- **Fast:** one directory in memory, 40-byte entries, lazy sorting, and a
  two-line redraw for cursor moves. Measured: ~3 ms to load and render a
  2,700-entry dir, ~12 ms for 20,000 entries, 4.4 MB RSS.
- **Safe:** no `unsafe`, no `Mutex`. Workers talk to the UI over channels and
  atomics only.
- **Plugins for preview:** any command that prints to stdout (ANSI colors
  supported) can be a previewer. Image and video previews, through
  [kitim](https://github.com/wensheng/kitim), on terminals with kitty graphics.

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
| `l` / `Enter`, `h` | enter dir / open file, parent |
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
nerd_font   = false       # file icons; enable with a Nerd Font in your terminal
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
kitim       = false       # image/video previews via kitim (see "Built-in plugin: kitim")
kitim_sound = false       # and play videos' sound in those previews
bat         = true        # highlight text previews with bat (or batcat), if installed

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
# graphics = true         # run prints kitty graphics, not text (see below)

[[opener]]                # Enter runs these fullscreen, file name appended
match = ["*.pdf", "*.PDF"]
run   = "kitpdf"

[[plugin]]
name     = "git status"
cmd      = "| git status" # see "Plugin modes" below
key      = "<f2>"         # optional direct key
requires = "git"          # optional: program(s) that must be on PATH
```

Environment: `VFM_SEL` (selection file path), `VFM_FCOLORS`
(`dir=red:link=6:exec=0x88cc88`), `VFM_NERD_FONT`, `NO_COLOR`.

For file-list icons, configure your terminal to use a [Nerd Font](https://www.nerdfonts.com/),
then set `[ui] nerd_font = true`. Icons appear before filenames in both detailed
and compact views, including Go (`U+E626`), Python (`U+E73C`), common file types,
directories, and symbolic links. Unknown file types use a generic file icon.
Selection marks, file colors, and `/`, `@`, `*` indicators still work as usual.

Font glyph coverage cannot be detected reliably across terminals, so icons are
off by default. `VFM_NERD_FONT=1` (or `true`) enables them for a session;
`VFM_NERD_FONT=0` (or `false`) disables them. This overrides the config file;
empty or unrecognized values leave the config unchanged. `NO_COLOR` affects
colors independently of icons.

## Plugin protocol

**Previewers** run as `<run…> PATH WIDTH HEIGHT`, with stdout piped. vfm parses
SGR colors and strips other escape sequences. With no matching rule, a built-in
preview shows text heads, directory listings, symlink targets, and binary file
types. If [bat](https://github.com/sharkdp/bat) is installed (as `bat` or
`batcat`), text heads are syntax-highlighted through it; `bat = false` under
`[preview]` keeps them plain. Moving the cursor kills a slow previewer
immediately; recent results are cached.

### Graphics previewers

A `[[previewer]]` with `graphics = true` draws images instead of text. It runs
the same way, as `<run…> PATH WIDTH HEIGHT`, where WIDTH and HEIGHT are the
pane's body in cells. vfm only uses such a rule if the terminal answered its
kitty graphics probe at startup, as terminals implementing the protocol do
(kitty and Ghostty, for example).
Elsewhere, and inside tmux, the next matching rule or the built-in preview is
used instead.

Its stdout is a stream of frames. Each frame is optional cursor moves (`CSI n
A`/`B`/`C`/`D`, relative to the pane's top-left cell) followed by one kitty
graphics `a=T` image, chunked with `m=1` on every chunk but the last. Each
frame should have its own `i=` image id: vfm puts a new frame up, then deletes
the previous one, so animations don't flicker. vfm forwards only those
sequences, and forces `q=2` and `C=1`, so a previewer can't move the screen or
make the terminal answer on vfm's input. Plain text that isn't part of a frame
is shown as the error when no frame arrives.

### Built-in plugin: kitim

vfm ships one graphics previewer, off by default. Install a
[kitim](https://github.com/wensheng/kitim) that has `--preview`, then enable it
in `config.toml`:

```toml
[preview]
kitim = true
```

That previews images (png, jpg, gif, webp, bmp, ico, tiff, tga, qoi, pnm) and
videos (mp4, m4v, mkv, webm, mov, avi, wmv, flv, mpg, mpeg, 3gp), in lower or
upper case. Animated GIFs and videos loop until you move the cursor. Videos
are muted unless you also set `kitim_sound = true`; then their sound plays,
and stops when you move off the file or vfm runs another program.

That's all the configuration needed. Behind the switch, vfm adds a rule like
the one below, after your own `[[previewer]]` rules. Don't copy it into your
config: a rule of yours wins for the files it matches, so it would replace
the built-in one there, `kitim_sound` included. Write one only to change
something, such as a kitim that isn't on `PATH` or other file types:

```toml
[[previewer]]
match    = ["*.png", "*.PNG", "*.jpg", "*.JPG", "*.mp4", "*.MP4"] # and the rest
run      = "kitim --preview"         # "kitim --sound --preview" with kitim_sound
graphics = true
```

### Opening files

Enter on a directory enters it; on an executable, runs it. Other files open
the first matching `[[opener]]` rule's `run` command fullscreen, with the
file's quoted name appended (`kitpdf photo.pdf`). With no matching rule:

- image, video and audio files open in
  [kitim](https://github.com/wensheng/kitim) fullscreen — images fill the
  screen and any key returns — when it is on `PATH` and the terminal
  answered the kitty-graphics probe,
- files whose head reads as text open in `$VISUAL`/`$EDITOR` (vi by default),
- anything else does nothing — no editor over binary bytes.

`e` always opens `$EDITOR` on the hovered file, whatever it is.

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
