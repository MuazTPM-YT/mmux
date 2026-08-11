//! mmux — Muaz Multiplexer.
//!
//! A terminal multiplexer for Arch Linux / Hyprland with a WASD-centric keymap
//! under an `Alt` master prefix and tmux-style detachable sessions.
//!
//! `mmux [session]` runs as a **client**: it attaches to the named session's
//! background **server** (spawning it on first use), which owns the shells so the
//! session survives detach (`Alt+Enter`). `mmux __server <name>` is the internal
//! daemon entry point and is not meant to be run directly.

mod app;
mod backend;
mod client;
mod keys;
mod protocol;
mod pty;
mod screen;
mod server;
mod session;
mod state;
mod ui;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("__server") => {
            let name = args.get(2).cloned().unwrap_or_else(|| "default".into());
            server::run(name).await
        }
        Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("mmux {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // An unrecognised flag is a typo, not a session name — creating a
        // session called "__foo" silently would be worse than saying so.
        Some(flag) if flag.starts_with('-') => {
            eprintln!("mmux: unknown option '{flag}' (try --help)");
            std::process::exit(2);
        }
        Some(name) => client::run(session::sanitize(name)).await,
        None => client::run("default".into()).await,
    }
}

fn print_help() {
    println!(
        "\
mmux {ver} — Muaz Multiplexer

USAGE:
    mmux [SESSION]        Attach to SESSION (default: \"default\"), creating it if needed
    mmux --help          Show this help
    mmux --version       Show version

SESSIONS:
    Sessions run in a background server that keeps your shells alive.
    Alt+Enter             Detach (shells keep running); reattach with `mmux SESSION`

KEYS (prefix = Alt):
    Alt+w/a/s/d           Focus pane up/left/down/right
    Alt+W/A/S/D           New pane up/left/down/right
    Alt+q                 Close pane      Alt+t  New space     Alt+r  Rename space
    Alt+1..9              Switch space    Alt+left/right  Cycle spaces
    Alt+Enter             Detach session",
        ver = env!("CARGO_PKG_VERSION")
    );
}
