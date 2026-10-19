# vfm

A small, fast terminal file manager, runs on Linux, macOS and Windows.

- **Small:** ~1 MB binary; crossterm, unicode-width, toml, regex-lite,
  trash, and a platform helper (libc on Unix, crossterm_winapi on Windows).
- **Fast:** one directory in memory, 40-byte entries, lazy sorting, and a
  two-line redraw for cursor moves. Measured: ~3 ms to load and render a
  2,700-entry dir, ~12 ms for 20,000 entries, 4.4 MB RSS.
- **Safe:** no `unsafe`, no `Mutex`. Workers talk to the UI over channels and
  atomics only.
- **Plugins for preview:** commands, arguments, file patterns, and terminal
  profiles come from configuration. Text with ANSI colors, Kitty graphics,
  and the Vivid side channel are supported. No external preview program is
  selected automatically.

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
run   = "my-text-preview --color" # your command or wrapper, accepts PATH WIDTH HEIGHT
# graphics = true         # run prints kitty graphics, not text (see below)
# vivid = true            # run draws through Vivid itself (see below)

[[opener]]                # Enter runs these fullscreen, file name appended
match = ["*.pdf", "*.PDF"]
run   = "my-document-reader"

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

Every external preview is an explicit rule with filename globs and a command.
vfm does not associate executables, options, or extensions with a terminal.
It selects a terminal profile, finds the first matching rule, and runs that
rule using the chosen output protocol.

`[[preview.kitty.previewer]]` configures terminals that answer the Kitty
graphics probe, including kitty, Ghostty, and cmux. Vivido and Vivida share
`[[preview.vivido.previewer]]`. Without a Vivido section, they inherit the
Kitty rules after answering the graphics probe. When a Vivido section exists,
its rules replace the Kitty profile, including for extensions it does not
list. An empty `[preview.vivido]` disables inheritance too.

For example, the following configuration chooses these commands for PDFs:

```toml
[[preview.kitty.previewer]]
match = ["*.pdf", "*.PDF"]
run = "kitpdf --preview"
graphics = true
interactive = true

[[preview.vivido.previewer]]
match = ["*.pdf", "*.PDF"]
run = "vvrd --preview"
vivid = true
interactive = true
```

The command names above are examples. Any program implementing the same
preview protocol can replace them, and any filename globs can be used.
Rules run in their listed order within the selected profile, before shared
top-level `[[previewer]]` rules. Shared rules remain fallbacks for every
terminal. Unmatched files use the built-in plain text, directory, symlink,
or binary summary preview. A missing configured program shows an error;
vfm does not substitute another executable.

See [config.example.toml](config.example.toml) for complete image, video,
document, and text rules using kitim, kitpdf, vivi, vvrd, and bat. All program
names, extension lists, audio flags, highlighting options, and line-number
options live in that file. Copy the rules you need and change the commands
and patterns to suit your installed programs.

The old program-specific switches under `[preview]`, `[preview.kitty]`, or
`[preview.vivido]` have been removed. Preview settings report unknown keys;
replace those switches with explicit `match` and `run` rules. Automatic
fullscreen media openers have also been removed: use `[[opener]]` rules.

## Plugin protocol

**Previewers** run as `<run…> PATH WIDTH HEIGHT`, with stdout piped. vfm parses
SGR colors and strips other escape sequences. With no matching rule, a built-in
preview shows text heads, directory listings, symlink targets, and binary file
types. Syntax highlighting is provided by an explicit text preview rule;
there is no automatic highlighter selection. Moving the cursor kills a slow
previewer immediately; recent results are cached.

Commands receive `PATH WIDTH HEIGHT` appended to their configured arguments.
Programs that do not accept those arguments need a wrapper. For example,
this POSIX shell rule passes only the path to bat:

```toml
[[previewer]]
match = ["*.rs", "*.toml", "*.txt"]
run = """sh -c 'exec bat --color=always --paging=never --wrap=never -n -- "$1"' sh"""
```

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

### Interactive document previews

Set `interactive = true` on a rule whose command accepts page commands.
Press `P` to show the preview, then `l`/Right to focus it. `j`/Down,
PageDown, Ctrl-f, or Right sends `next`; `k`/Up, PageUp, or Ctrl-b sends
`prev`. Home/End sends `first`/`last`. Esc or `h`/Left returns focus to the
file list. Enter retains its normal open-file action.

The configured previewer handles page boundaries, rendering, reflow, and
supported document types. Selecting another file or resizing starts a new
preview using the matching rule. vfm does not add program-specific flags.

### Opening files

Enter on a directory enters it; on an executable, runs it. Other files open
the first matching `[[opener]]` rule's `run` command fullscreen, with the
file's quoted name appended. With no matching rule, files whose head reads
as text open in `$VISUAL`/`$EDITOR` (vi by default); binary files stay
unopened. No media or document program is selected automatically.

```toml
[[opener]]
match = ["*.pdf", "*.PDF"]
run = "my-document-reader --fullscreen"
```

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
