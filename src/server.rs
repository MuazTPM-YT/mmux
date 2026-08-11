//! The per-session **server**: a background daemon that owns the shells and the
//! emulator, and renders the UI into escape bytes streamed to whichever client
//! is attached. It keeps running while no client is attached (that is what makes
//! detach/attach work) and exits only when its last shell closes.

use std::time::Duration;

use anyhow::Result;
use ratatui::Terminal;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{channel, Sender};
use tokio::time::{sleep_until, Instant};

use crate::app::App;
use crate::backend::VirtualBackend;
use crate::protocol::{read_msg, write_msg, ClientMsg, ServerMsg};
use crate::pty::PTY_QUEUE;
use crate::state::Manager;
use crate::{session, ui};

/// Minimum gap between frames sent to the client.
///
/// Without a floor, a pane dumping output (a build log, `cat` of a big file)
/// makes the loop render and ship a frame per 8 KB chunk. The client's real
/// terminal cannot draw that fast, so frames queue up and the screen tears and
/// lags behind the shell. Coalescing to ~125 fps is imperceptible for typing and
/// turns a flood into a handful of frames.
const MIN_FRAME_GAP: Duration = Duration::from_millis(8);

/// How long a fresh connection has to identify itself before we drop it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

struct Client {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

/// A connection that has completed its handshake, handed to the main loop.
enum Incoming {
    Attach {
        cols: u16,
        rows: u16,
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    },
    /// A one-shot control message (`Shutdown` / `Rename`) from the switcher.
    Control(ClientMsg),
}

/// Await the next message from the attached client, or never resolve if none.
async fn next_msg(client: &mut Option<Client>) -> std::io::Result<ClientMsg> {
    match client {
        Some(c) => read_msg(&mut c.reader).await,
        None => std::future::pending().await,
    }
}

/// Read a new connection's first message off the main loop.
///
/// This deliberately runs in its own task. Doing the read inline would let any
/// connection that stalls — a half-open probe, a client that dies mid-handshake
/// — freeze the whole session for the length of the timeout: no keystrokes, no
/// shell output, no rendering.
fn spawn_handshake(stream: UnixStream, tx: Sender<Incoming>) {
    tokio::spawn(async move {
        let (mut r, w) = stream.into_split();
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_msg::<_, ClientMsg>(&mut r)).await;
        let msg = match first {
            Ok(Ok(ClientMsg::Attach { cols, rows })) => Incoming::Attach {
                cols,
                rows,
                reader: r,
                writer: w,
            },
            Ok(Ok(msg @ (ClientMsg::Shutdown | ClientMsg::Rename(_)))) => Incoming::Control(msg),
            // A liveness probe (`session::is_active` connects and drops), a
            // timeout, or junk — nothing to do.
            _ => return,
        };
        let _ = tx.send(msg).await;
    });
}

/// Bind the session socket, refusing to steal it from a healthy server.
///
/// Unconditionally removing the socket first (the obvious approach) loses a race
/// two clients can trigger by starting the same session at once: the second
/// daemon deletes the first one's socket, so the first keeps running with its
/// shells permanently unreachable.
fn bind_socket(name: &str) -> Result<Option<UnixListener>> {
    let path = session::socket_path(name);
    match UnixListener::bind(&path) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // `is_active` connects to tell a live server from a stale socket
            // file, and cleans the file up if nobody answers.
            if session::is_active(name) {
                return Ok(None); // someone else already serves this session
            }
            let _ = std::fs::remove_file(&path);
            Ok(Some(UnixListener::bind(&path)?))
        }
        Err(e) => Err(e.into()),
    }
}

/// Wait on a signal stream, or never resolve if it couldn't be registered.
async fn wait_signal(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// Sleep until a coalesced frame is due, or never if none is pending.
async fn wait_until(due: Option<Instant>) {
    match due {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}

pub async fn run(mut name: String) -> Result<()> {
    let Some(mut listener) = bind_socket(&name)? else {
        return Ok(()); // another server won the race; nothing to do
    };

    // Build the app from the session's saved spaces (fresh shells).
    let (tx, rx) = channel(PTY_QUEUE);
    let spaces = session::load(&name);
    let mgr = Manager::new(tx, &spaces)?;
    let mut app = App::new(mgr, rx);

    // Persist the current layout so the switcher can see this session.
    session::save(&name, &app.mgr.space_names());
    let mut last_gen = app.mgr.spaces_gen();

    let mut terminal = Terminal::new(VirtualBackend::new(80, 24))?;
    let _ = terminal.hide_cursor();

    let (conn_tx, mut conn_rx) = channel::<Incoming>(8);

    // Terminate cleanly on SIGTERM/SIGHUP so the shells are killed and reaped
    // (via `Manager`'s drop) instead of being orphaned by an abrupt exit.
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sighup = signal(SignalKind::hangup()).ok();

    let mut client: Option<Client> = None;
    let mut dirty = false;
    // `None` = nothing drawn yet, so the first frame goes out immediately.
    let mut last_render: Option<Instant> = None;
    // Control-message flags, applied after the select! to avoid borrowing the
    // listener while rebinding it.
    let mut shutdown = false;
    let mut rename_to: Option<String> = None;

    loop {
        if app.is_empty() || shutdown {
            break; // last shell closed (or Shutdown) — destroy the session
        }

        // Render, but never more often than MIN_FRAME_GAP; if a frame is due
        // later, `frame_due` becomes an extra wake-up source below.
        let mut frame_due = None;
        if dirty && client.is_some() {
            let due = last_render.map(|t| t + MIN_FRAME_GAP);
            if due.is_none_or(|d| Instant::now() >= d) {
                if let Some(c) = &mut client {
                    let _ = terminal.draw(|f| ui::draw(f, &mut app.mgr, &app.mode, &name));
                    // Panes only learn their size from a render, so a layout
                    // change means the frame we just built used the previous
                    // grid shape. Draw again so the client never sees the
                    // mis-shaped one; the two diffs concatenate cleanly.
                    if app.sync_sizes() {
                        let _ = terminal.draw(|f| ui::draw(f, &mut app.mgr, &app.mode, &name));
                    }
                    let bytes = terminal.backend_mut().take_output();
                    if !bytes.is_empty()
                        && write_msg(&mut c.writer, &ServerMsg::Frame(bytes)).await.is_err()
                    {
                        client = None;
                    }
                    last_render = Some(Instant::now());
                    dirty = false;
                }
            } else {
                frame_due = due;
            }
        }

        // Persist only when the set of spaces actually changed (no per-event
        // clone of every space name on the hot path).
        if app.mgr.spaces_gen() != last_gen {
            session::save(&name, &app.mgr.space_names());
            last_gen = app.mgr.spaces_gen();
        }

        tokio::select! {
            // Wake up when a coalesced frame comes due.
            _ = wait_until(frame_due) => {}

            conn = listener.accept() => {
                if let Ok((stream, _)) = conn {
                    spawn_handshake(stream, conn_tx.clone());
                }
            }
            incoming = conn_rx.recv() => {
                match incoming {
                    Some(Incoming::Attach { cols, rows, reader, writer }) => {
                        client = Some(Client { reader, writer });
                        resize_terminal(&mut terminal, cols, rows);
                        dirty = true;
                    }
                    Some(Incoming::Control(ClientMsg::Shutdown)) => shutdown = true,
                    Some(Incoming::Control(ClientMsg::Rename(new))) => rename_to = Some(new),
                    _ => {}
                }
            }
            msg = next_msg(&mut client) => {
                match msg {
                    Ok(ClientMsg::Attach { cols, rows }) | Ok(ClientMsg::Resize { cols, rows }) => {
                        resize_terminal(&mut terminal, cols, rows);
                        dirty = true;
                    }
                    Ok(ClientMsg::Redraw) => {
                        let _ = terminal.clear();
                        dirty = true;
                    }
                    Ok(ClientMsg::Key(k)) => {
                        if let Some(ev) = k.to_event() {
                            if app.handle_terminal_event(crossterm::event::Event::Key(ev)) {
                                dirty = true;
                            }
                        }
                    }
                    Ok(ClientMsg::Paste(text)) => app.paste(&text),
                    Ok(ClientMsg::Shutdown) => shutdown = true,
                    Ok(ClientMsg::Rename(new)) => rename_to = Some(new),
                    Ok(ClientMsg::Detach) | Err(_) => {
                        // Client left; keep running so the shells stay alive.
                        client = None;
                    }
                }
            }
            ev = app.recv_pty() => {
                if let Some(e) = ev {
                    app.drain_pty(e);
                    dirty = true;
                }
            }
            _ = wait_signal(&mut sigterm) => shutdown = true,
            _ = wait_signal(&mut sighup) => shutdown = true,
        }

        // Apply a rename now that the listener is no longer borrowed.
        if let Some(new) = rename_to.take() {
            let new = session::sanitize(&new);
            if new != name {
                if let Ok(Some(l)) = bind_socket(&new) {
                    session::save(&new, &app.mgr.space_names());
                    session::purge(&name); // remove the old record + socket
                    listener = l; // rebind; the attached connection is unaffected
                    name = new;
                    last_gen = app.mgr.spaces_gen();
                    dirty = true; // the tab bar shows the new name
                }
            }
        }
    }

    // Session destroyed: tell the client, then remove socket + record. Dropping
    // `app` kills and reaps every shell (see `PtyProcess::drop`).
    if let Some(c) = &mut client {
        let _ = write_msg(&mut c.writer, &ServerMsg::Bye).await;
    }
    session::purge(&name);
    Ok(())
}

/// Point the virtual backend at the client's terminal size and force a full
/// repaint, since the client's screen no longer matches our previous frame.
fn resize_terminal(terminal: &mut Terminal<VirtualBackend>, cols: u16, rows: u16) {
    terminal.backend_mut().set_size(cols, rows);
    let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, cols.max(1), rows.max(1)));
    let _ = terminal.clear();
}
