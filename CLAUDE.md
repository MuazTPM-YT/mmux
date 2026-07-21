# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo run -- work            # attach to a session named "work" (needs a real TTY)
cargo build --release        # release build
cargo test                   # all unit tests
cargo test session::         # one module's tests (also screen::, keys::, protocol::)
cargo clippy                 # lint (kept warning-clean)
```

Requires a real terminal; in a non-TTY the client exits cleanly. Exercise it
headlessly by driving `mmux <name>` through a PTY (`pty.fork()` in Python) with
`MMUX_RUNTIME_DIR`/`MMUX_DATA_DIR` pointed at temp dirs, and inspect the emitted
bytes — that is how detach/attach and the switcher are verified end-to-end.

## Architecture

mmux is **client/server** (tmux-style). `main.rs` dispatches:
- `mmux [name]` → `client::run` — owns the real terminal.
- `mmux __server <name>` → `server::run` — the per-session background daemon
  (spawned by the client via `setsid`; not run by hand).

**Server (`server.rs`)** owns all state and does all rendering. It builds an
`App` (the reducer in `app.rs`) over a `Manager` of PTY-backed panes, renders the
whole UI into terminal escape bytes with a fixed-size `VirtualBackend`
(`backend.rs`), and streams them as `ServerMsg::Frame` over a Unix socket. It
keeps running with no client attached (that is what makes detach work) and exits
only when its last shell closes. A freshly accepted connection is held in
`pending` until it sends `Attach` — so liveness probes (`session::is_active`,
which just connects and disconnects) never disturb the attached client.

**Client (`client.rs`)** owns the tty: it pipes server frames to the screen and
forwards input as `ClientMsg`. It intercepts only the global chords — `Alt+Enter`
(detach), `Alt+.` (list), `Alt+/` (search) — and renders the switcher overlays
itself (`client/overlay.rs`), repainting the session via `ClientMsg::Redraw` or
reconnecting to another server on switch.

**Protocol (`protocol.rs`)** — length-prefixed postcard messages. `WireKey`
mirrors a `crossterm::KeyEvent`; frames are raw escape bytes.

**Sessions (`session.rs`)** — a session is identified by its sanitised name, with
a socket in the runtime dir and a record (name + space names) in the data dir.
"active" = a server is listening (verified by connecting). Base dirs are
overridable via `MMUX_RUNTIME_DIR` / `MMUX_DATA_DIR` (used by tests + E2E).

**App reducer (`app.rs`).** UI- and transport-agnostic: pure step functions
(`handle_terminal_event`, `drain_pty`, `sync_sizes`) that the server drives. A
single `tokio::select!` in the server waits on two sources:
- client input (over the socket), and
- PTY output via a `tokio::mpsc` channel.

After every wake-up the whole UI is redrawn and pane sizes are reconciled. PTY
readers are **blocking `std::io`**, so each pane gets a dedicated OS thread
(`pty.rs`) that pumps bytes into the async channel tagged with a pane id — the
loop never blocks on a slow child.

**Data flow for shell output:** PTY reader thread → `mpsc` channel → `App` →
`Pane::feed` → `vte::Parser` → `Screen` (a cell grid) → `ui::blit_screen` copies
cells straight into ratatui's back buffer. `Screen` (`screen.rs`) *is* the
terminal emulator: it implements `vte::Perform` (print, CSI cursor/erase/SGR,
scroll region, OSC titles). Extend emulation by adding cases there.

**Query responses (important).** Some escape sequences are *queries* the shell
sends and then **blocks on** — notably Device Attributes (`CSI c` / `CSI > c`)
and Device Status Report (`CSI n`). A terminal that never answers makes shells
stall: fish waits ~10s for the DA1 reply before drawing its prompt. `Screen`
handles these by pushing the reply bytes into `Screen::responses`, which
`Pane::feed` drains and writes back to the PTY after each parse. If you add any
new query handling, follow that pattern — never leave a query unanswered.

**CSI intermediates matter.** `csi_dispatch` answers DA/DSR first, then **returns
early for any CSI carrying an intermediate/private marker** (`>`, `?`, …). This is
load-bearing: `ESC[>4;2m` (modifyOtherKeys) and `ESC[>1u` (Kitty keyboard) share
final bytes with SGR (`m`) and DECRC (`u`); without the guard they were misparsed
— `>4;2m` set SGR "4" (underline) and stuck it on, which is what made Claude Code
render everything underlined. Only intermediate-free CSIs reach the SGR/cursor arms.

**Data flow for keystrokes (`keys.rs`):** the prefix is **`Alt`** (a modifier
chord). `parse_command` requires `ALT`; lowercase `wasd` → `Focus(dir)`, uppercase
`WASD` (Shift) → `NewPane(dir)`; `q/t/r`, digits, and arrows map to the space/pane
commands. Anything unmatched goes to `encode_key` → bytes written to the focused
PTY. "Shift" = uppercase char OR the SHIFT flag, because terminals consume Shift
when it produces a shifted glyph (`Alt+Shift+1` arrives as `Alt+!`).

**State (`state.rs`).** `Manager` owns every `Pane` in a `HashMap<u64, Pane>` and
a `Vec<Space>` (tabs). Each `Space` holds a **binary layout tree** (`Node::Leaf` /
`Node::Split{dir, ratio}`) plus the focused pane id. `new_pane(dir)` replaces the
focused leaf with a split (Left/Right → horizontal, Up/Down → vertical; the side
sets child order); closing collapses the split back to its sibling and drops the
space if it was the last pane. Directional focus (`focus_dir`) runs off the pane
rectangles captured during the **previous** render (`Pane::rect`), which is why it
works for arbitrary trees — `sync_sizes` and `focus_dir` need fresh rects, so they
are only valid after a draw.

**Performance model.** Event-driven, no timers: blocks in `tokio::select!` on
input or PTY output. It redraws **only when state changed** — a keystroke merely
forwarded to the shell returns `false` from the handlers and is not redrawn; the
shell's echo (PTY output) triggers the one redraw. So idle CPU is zero and typing
renders once, not twice. Each `Screen` stores cells in one contiguous row-major
`Vec<Cell>` (not `Vec<Vec<Cell>>`): scrolling is `copy_within` (memmove), fills
are slice `fill`s, `Cell` is `Copy`. Access cells via `Screen::cell(x, y)`.

## Conventions

- Splits: `SplitDir::Horizontal` = children **side by side**;
  `SplitDir::Vertical` = **stacked**. This matches the arrangement, not the
  divider orientation — keep it consistent.
- The terminal is always restored on exit *and* on panic (a panic hook in
  `main.rs` calls `restore_terminal`). Any new early-exit path must not bypass it.
