<div align="center">

# mmux

**A blazingly-fast terminal multiplexer with tmux-style detachable sessions —
built from scratch in Rust for Arch Linux + Hyprland.**

![Rust](https://img.shields.io/badge/Rust-1.75%2B-b7410e?logo=rust&logoColor=white)
![Platform](https://img.shields.io/badge/platform-Linux-1793d1?logo=linux&logoColor=white)
![License](https://img.shields.io/badge/license-MIT-green)

</div>

---

mmux splits your terminal into **panes** and **spaces** (tabs), keeps your shells
running in a background server so you can **detach and reattach** at will, and
drives all of it from an ergonomic **`Alt` + WASD** keymap chosen to never
collide with Hyprland's `$mainMod`.

## Features

- **🚀 Detachable sessions** — shells run in a per-session background daemon.
  Detach with `Alt+Enter`, close your terminal, come back with `mmux <name>` and
  your shells are exactly as you left them.
- **🔀 Session switcher** — `Alt+.` lists every session (active or passive) to
  open, delete, rename, or create; `Alt+/` fuzzy-searches them by name.
- **🪟 WASD panes** — `Alt+wasd` moves focus, `Alt+WASD` splits a new pane in
  that direction. Arbitrary nested splits.
- **🗂 Spaces** — tmux-style tabs, `Alt+1..9` to jump, `Alt+←/→` to cycle.
- **🧘 Out of the way** — panes are frameless and there is no status bar;
  `Alt+h` shows the keys when you need them.
- **⚡ Fast & lean** — fully event-driven (zero idle CPU), a contiguous cell
  grid with `memmove` scrolling, and diffed rendering over a local socket.
- **🎯 Real terminal emulation** — VTE-based, answers DA/DSR queries (no shell
  start-up stalls), 256-colour + truecolor, dim/bold/underline/reverse.

## Install

**One-liner:**

```sh
curl -fsSL https://raw.githubusercontent.com/MuazTPM-YT/mmux/main/install.sh | sh
```

**From a checkout:**

```sh
git clone https://github.com/MuazTPM-YT/mmux && cd mmux
./install.sh          # builds release + installs to ~/.local/bin
```

**With cargo:**

```sh
cargo install --path .
```

Make sure the install dir is on your `PATH` (`install.sh` prints the exact line
for your shell — e.g. `fish_add_path ~/.local/bin`).

## Quick start

```sh
mmux              # attach to the "default" session (created on first use)
mmux work         # attach to a session named "work"
```

Inside mmux:

1. `Alt+D` opens a pane to the right, `Alt+S` one below — `Alt+wasd` moves focus.
2. `Alt+Enter` detaches. Your shells keep running.
3. `mmux work` later drops you right back where you were.

## Sessions

A **session** is a named workspace backed by a background **server** that owns
its shells. This is what makes detach/attach work: the client (`mmux <name>`) only
draws and forwards keys — closing it never kills your shells.

| Chord | Action |
|-------|--------|
| `Alt+Enter` | **Detach** — leave the session running, exit the client |
| `Alt+.` | **List** sessions — `w/s` move, `Enter` open, `D` delete, `R` rename, `N` new |
| `Alt+/` | **Search** sessions by name — type to filter, `Enter` open (or create) |

A session is destroyed automatically when its last shell exits. Passive sessions
(saved but not running) reopen with fresh shells; active ones reattach live.

## Keybindings

Prefix is **`Alt`**. Lowercase `wasd` focuses; uppercase `WASD` (`Alt+Shift`)
creates a pane.

| Chord | Action |
|-------|--------|
| `Alt + w / a / s / d` | Focus pane up / left / down / right |
| `Alt + W / A / S / D` | New pane up / left / down / right |
| `Alt + q` | Close the focused pane |
| `Alt + t` | New space |
| `Alt + r` | Rename the current space |
| `Alt + 1 … 9` | Jump to space N |
| `Alt + ← / →` | Previous / next space |
| `Alt + Enter` | Detach session |
| `Alt + .` / `Alt + /` | Session list / search |
| `Alt + h` | Show all keybindings (any key closes) |

## Configuration

mmux is configured through the environment:

| Variable | Purpose | Default |
|----------|---------|---------|
| `SHELL` | Shell launched in each pane | `/bin/bash` |
| `MMUX_DATA_DIR` | Where session records live | `$XDG_DATA_HOME/mmux` |
| `MMUX_RUNTIME_DIR` | Where session sockets live | `$XDG_RUNTIME_DIR/mmux` |

## How it works

```
  mmux <name>  ─────────────►  server  (background daemon, per session)
   (client)     Unix socket    ├─ owns the PTYs / shells
   owns the tty                ├─ VTE terminal emulator per pane
   forwards keys ◄──frames───  └─ renders the UI to escape bytes
```

- The **server** owns the `Manager` (spaces, a binary split-tree of panes, and
  the emulator screens). It renders the whole UI into terminal escape bytes at
  the client's size and streams them over a Unix socket.
- The **client** owns your real terminal: it pipes those bytes to the screen and
  forwards input. It intercepts only the global chords (`Alt+Enter`, `Alt+.`,
  `Alt+/`) to drive detach and the session switcher.

See [`CLAUDE.md`](./CLAUDE.md) for a deeper architecture tour.

## Development

```sh
cargo run -- work     # run a session directly
cargo test            # unit suite (emulator, layout, keymap, protocol, sessions)
cargo clippy          # lint (kept warning-clean)
```

## Limitations

- Linux only (uses Unix sockets, `/proc`, `setsid`).
- The terminal emulator covers what shells and common tools need; very heavy
  full-screen apps (e.g. `vim`, `htop`) may render imperfectly.
- Sessions preserve **live** shells across detach while the server runs; a reboot
  (which kills all servers) turns them passive — they reopen with fresh shells.

## License

MIT © Muaz Ismail Mohammed — see [`LICENSE`](./LICENSE).
