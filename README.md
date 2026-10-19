# vfm

A small, fast terminal file manager, runs on Linux, macOS and Windows.

- **Small:** ~1 MB binary; crossterm, unicode-width, toml, regex-lite,
  trash, and a platform helper (libc on Unix, crossterm_winapi on Windows).
- **Fast:** one directory in memory, 40-byte entries, lazy sorting, and a
  two-line redraw for cursor moves. Measured: ~3 ms to load and render a
  2,700-entry dir, ~12 ms for 20,000 entries, 4.4 MB RSS.
- **Safe:** no `unsafe`, no `Mutex`. Workers talk to the UI over channels and
  atomics only.
- **Plugins for preview:** any command that prints to stdout (ANSI colors
  supported) can be a previewer. Image and video previews, through
  [kitim](https://github.com/wensheng/kitim), and PDF/EPUB previews through
  [kitpdf](https://github.com/wensheng/kitpdf), on terminals with kitty graphics.
  Vivido and Vivida can use a separate profile with
  [vivi](https://github.com/vivido-dev/vivi) and
  [vvrd](https://github.com/vivido-dev/vvrd).

## Install

```sh
cargo install vfm
```

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
| `<` / `>`, or drag the divider | move the preview divider left / right (widen / narrow the preview) |
| `Right` / `l` on a text file | focus the visible preview pane |
| `Up` / `Down` while preview is focused | scroll preview by one displayed row |
| `Page Down` / `C-f`, `Page Up` / `C-b` while preview is focused | scroll preview one page down / up |
| `Esc` / `Left` while preview is focused | return focus to the file list |
| `t` | tree view of the highlighted folder in the preview pane (needs `tree` on Unix; built into Windows) |
| `;` | plugin menu |
| `q`, `C-q`, `C-g` | close context, quit, quit and cd |

The focused preview has a highlighted divider and a `FOCUSED` header. Scrolling
stops at the beginning or end of the loaded preview; large previews remain
subject to the configured byte limit. `Enter` still opens the file.

`Esc` first leaves preview focus, otherwise cancels a running file op, closes
a plugin's preview, clears an active filter, or closes an overlay.

`<` and `>` move the divider by 5% of the terminal width; dragging it with
the mouse moves it to the pointer. Either side keeps at least 20 columns, and
the split is kept as a share of the width when the terminal resizes. Text
rewraps as the divider moves; previewers that size their output to the pane,
such as image previews, rerun when the drag ends. Resizing keeps a focused
preview focused. `[ui] preview_width` sets the starting split, and `[keys]`
can rebind `preview_wider` and `preview_narrower`.

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
preview_width = 50        # preview pane's share of the width, in percent (10-90)
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
bat         = true        # highlight text previews with bat (or batcat), if installed
line_numbers = true       # use bat -n; false uses bat -p without line numbers

[preview.kitty]          # Kitty graphics terminals: kitty, Ghostty, cmux, etc.
kitim       = false       # image/video previews via kitim
kitim_sound = false       # play videos' sound in those previews
kitpdf      = false       # PDF/EPUB previews with page navigation

# Optional: omit this section to use the Kitty settings in Vivido/Vivida.
[preview.vivido]         # Vivido and Vivida share this profile
vivi        = false       # image/video previews via vivi
vivi_sound  = false       # play videos' sound in those previews
vvrd        = false       # PDF/EPUB/SVG/Markdown previews with page navigation

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
# vivid = true            # run draws through Vivid itself (see below)

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

### Terminal-specific previews

`[preview.kitty]` configures terminals that answer the Kitty graphics probe,
including kitty, Ghostty, and cmux. Vivido and Vivida form a separate category
and use `[preview.vivido]` when it exists. If that section is absent, they use
the Kitty settings after answering the same graphics probe. An empty Vivido
section disables this inheritance; omitted or disabled viewers in an explicit
Vivido section do not fall back to their Kitty counterparts.

For example, this config uses kitim and kitpdf in Kitty graphics terminals,
and vivi and vvrd in Vivido or Vivida:

```toml
[preview.kitty]
kitim = true
kitpdf = true

[preview.vivido]
vivi = true
vvrd = true
```

Each profile also accepts custom rules. Rules run in their listed order,
before that profile's built-in viewers. The selected profile takes precedence
over shared top-level `[[previewer]]` rules, which remain fallbacks in every
terminal. Use `graphics = true` for Kitty output and `vivid = true` for the
Vivid side channel; the profile selects the terminal independently of the
output protocol.

```toml
[[preview.kitty.previewer]]
match = ["*.pdf", "*.PDF"]
run = "/path/to/kitpdf --preview"
graphics = true
interactive = true

[[preview.vivido.previewer]]
match = ["*.pdf", "*.PDF"]
run = "/path/to/vvrd --preview"
vivid = true
interactive = true
```

The older `[preview] kitim`, `kitim_sound`, and `kitpdf` switches still work as
Kitty defaults, after shared custom rules. `[preview.kitty]` overrides those
switches and moves its viewers ahead of shared rules. These switches no longer
automatically enable vivi or vvrd; enable them explicitly in `[preview.vivido]`.

## Plugin protocol

**Previewers** run as `<run…> PATH WIDTH HEIGHT`, with stdout piped. vfm parses
SGR colors and strips other escape sequences. With no matching rule, a built-in
preview shows text heads, directory listings, symlink targets, and binary file
types. If [bat](https://github.com/sharkdp/bat) is installed (as `bat` or
`batcat`), text previews are syntax-highlighted with line numbers (`bat -n`)
by default. Set `line_numbers = false` under `[preview]` to hide the numbers
(`bat -p`), or `bat = false` to use the built-in plain preview. Moving the cursor
kills a slow previewer immediately; recent results are cached.

### Graphics previewers

A `[[previewer]]` with `graphics = true` draws images instead of text. It runs
the same way, as `<run…> PATH WIDTH HEIGHT`, where WIDTH and HEIGHT are the
pane's body in cells. vfm only uses such a rule if the terminal answered its
kitty graphics probe at startup, as terminals implementing the protocol do
(kitty, Ghostty, and cmux, for example).
Windows consoles use the same query through VT input, restoring native keyboard
input afterward; this includes Kitty-capable ConPTY terminals such as Vivida.
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

With `interactive = true` (requires `graphics = true` or `vivid = true`), stdin stays open.
While focused, vfm sends newline-terminated `next`, `prev`, `first`, and `last`
commands. The previewer clamps page boundaries and emits a new frame for each
page change. Pipe writes run off the UI thread with a bounded queue; excess
key repeats can be dropped while a previewer is stalled. Closing the pane,
changing the hovered file, or resizing stops the child and closes its pipes.

### Vivid previewers

A `[[previewer]]` with `vivid = true` draws its own picture through the Vivid
side channel, so vfm reads nothing from its stdout. vfm only uses such a rule
inside Vivido or Vivida, recognized by `TERM_PROGRAM` (`vivido`, `vivida`, or
`vivid`, case-insensitive), or by both `VIVID_ENDPOINT_CONTROL` and
`VIVID_ROOT_SECRET`. First matching rule wins within the selected profile.
It runs as
`<run…> PATH WIDTH HEIGHT COLUMN ROW`: the pane's body size, then its top-left
cell as a 0-based grid position in the window, for a grid-positioned node.

The previewer keeps its picture up until its stdin closes, then removes it and
exits; vfm kills it if it is still running 300 ms later. Stdin carries
newline-terminated `hide` and `show` while a menu or the pager covers the pane,
and, with `interactive = true`, the page commands above. If the previewer exits
on its own, the first line of its stderr is shown as the error. The terminal
draws text over Vivid media, so vfm leaves the pane body blank meanwhile.

Inside tmux or screen, Vivid grid positions are the outer window's, not the
pane's, so pictures land in the wrong place there.

### Built-in plugin: kitpdf

Build and install the companion kitpdf checkout with pane support:

```sh
cargo install --path ../kitpdf
```

Run that from the vfm checkout, then add this to `config.toml`:

```toml
[preview.kitty]
kitpdf = true
```

Press `P` to show the preview, select a PDF or EPUB, and press `l`/Right to
focus it. `j`/Down, PageDown, Ctrl-f, or Right turn to the next page;
`k`/Up, PageUp, or Ctrl-b turn back. Home/End jump to the first/last page.
Esc or `h`/Left returns focus to the file list. The focused header shows the
page controls. Enter retains its normal open-file action.

The plugin starts at page 1, keeps the document open for page turns, and
reflows EPUB text for the pane. Resizing or selecting another file starts
a fresh preview. It requires a terminal that answers vfm's Kitty graphics
probe; other terminals keep their normal fallback preview. Missing kitpdf
or unreadable documents show an error in the pane.

To use [vvrd](https://github.com/vivido-dev/vvrd)'s pane mode inside Vivido or
Vivida, install a vvrd with `--preview` on `PATH` and add:

```toml
[preview.vivido]
vvrd = true
```

This previews PDF, EPUB, SVG, and Markdown files. Without the Vivido section,
Vivido and Vivida use kitpdf through Kitty graphics. Focus and page keys work
the same way, and Enter opens the file in vvrd full screen when installed.

The built-in rules are added after custom rules in their profile. To use a
specific executable, configure the equivalent rule instead:

```toml
[[preview.kitty.previewer]]
match = ["*.pdf", "*.PDF", "*.epub", "*.EPUB"]
run = "kitpdf --preview"
graphics = true
interactive = true
```

For Vivido and Vivida, configure vvrd's rule in their profile:

```toml
[[preview.vivido.previewer]]
match = ["*.pdf", "*.PDF", "*.epub", "*.EPUB", "*.svg", "*.SVG", "*.md", "*.MD"]
run = "vvrd --preview"
vivid = true
interactive = true
```

### Built-in plugin: kitim

The image/video graphics previewer is off by default. Install a
[kitim](https://github.com/wensheng/kitim) that has `--preview`, then enable it
in `config.toml`:

```toml
[preview.kitty]
kitim = true
```

That previews images (png, jpg, gif, webp, bmp, ico, tiff, tga, qoi, pnm) and
videos (mp4, m4v, mkv, webm, mov, avi, wmv, flv, mpg, mpeg, 3gp), in lower or
upper case. Animated GIFs and videos loop until you move the cursor. Videos
are muted unless you also set `kitim_sound = true`; then their sound plays,
and stops when you move off the file or vfm runs another program.

To use [vivi](https://github.com/vivido-dev/vivi)'s pane mode inside Vivido or
Vivida, install a vivi with `--preview` on `PATH` and add:

```toml
[preview.vivido]
vivi = true
vivi_sound = false       # true runs vivi --sound --preview
```

Without the Vivido section, Vivido and Vivida use kitim through Kitty graphics.

Behind each switch, vfm adds a rule like the ones below, after your own rules
in that profile. Don't copy them into your config: a rule of yours wins for
the files it matches, so it replaces the built-in one there, including the
profile's sound setting. Write one only to change something, such as a viewer
that isn't on `PATH` or other file types:

```toml
[[preview.vivido.previewer]]         # Vivido and Vivida only
match    = ["*.png", "*.PNG", "*.jpg", "*.JPG", "*.mp4", "*.MP4"] # and the rest
run      = "vivi --preview"          # "vivi --sound --preview" with vivi_sound
vivid    = true

[[preview.kitty.previewer]]
match    = ["*.png", "*.PNG", "*.jpg", "*.JPG", "*.mp4", "*.MP4"] # and the rest
run      = "kitim --preview"         # "kitim --sound --preview" with kitim_sound
graphics = true
```

### Opening files

Enter on a directory enters it; on an executable, runs it. Other files open
the first matching `[[opener]]` rule's `run` command fullscreen, with the
file's quoted name appended (`kitpdf photo.pdf`). With no matching rule:

- inside Vivido, PDF, EPUB, SVG and Markdown files open in vvrd, and image,
  video and audio files in `vivi --fullscreen`, when those are on `PATH`
  (`e` still edits Markdown),
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
current directory. On Windows, vfm uses `%SystemRoot%\System32\tree.com` directly
(normally `C:\Windows\System32\tree.com`) with `/F /A`, regardless of `PATH`.
Paths passed in the plugin environment use standard drive/UNC notation instead
of Rust's canonical `\\?\` prefix, which Windows tree does not accept.

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
