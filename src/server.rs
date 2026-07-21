//! The per-session **server**: a background daemon that owns the shells and the
//! emulator, and renders the UI into escape bytes streamed to whichever client
//! is attached. It keeps running while no client is attached (that is what makes
//! detach/attach work) and exits only when its last shell closes.

use anyhow::Result;
use ratatui::Terminal;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;
use tokio::sync::mpsc::unbounded_channel;

use crate::app::App;
use crate::backend::VirtualBackend;
use crate::protocol::{read_msg, write_msg, ClientMsg, ServerMsg};
use crate::state::Manager;
use crate::{session, ui};

struct Client {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

/// Await the next message from the attached client, or never resolve if none.
async fn next_msg(client: &mut Option<Client>) -> std::io::Result<ClientMsg> {
    match client {
        Some(c) => read_msg(&mut c.reader).await,
        None => std::future::pending().await,
    }
}

pub async fn run(mut name: String) -> Result<()> {
    // Clear any stale socket, then bind.
    let _ = std::fs::remove_file(session::socket_path(&name));
    let mut listener = UnixListener::bind(session::socket_path(&name))?;

    // Build the app from the session's saved spaces (fresh shells).
    let (tx, rx) = unbounded_channel();
    let spaces = session::load(&name);
    let mgr = Manager::new(tx, &spaces)?;
    let mut app = App::new(mgr, rx);

    // Persist the current layout so the switcher can see this session.
    session::save(&name, &app.mgr.space_names());
    let mut last_gen = app.mgr.spaces_gen();

    let mut terminal = Terminal::new(VirtualBackend::new(80, 24))?;
    terminal.hide_cursor()?;

    let mut client: Option<Client> = None;
    let mut attached = false;
    let mut dirty = false;
    // Control-message flags, applied after the select! to avoid borrowing the
    // listener while rebinding it.
    let mut shutdown = false;
    let mut rename_to: Option<String> = None;

    loop {
        if app.is_empty() || shutdown {
            break; // last shell closed (or Shutdown) — destroy the session
        }

        if dirty && attached {
            if let Some(c) = &mut client {
                terminal.draw(|f| ui::draw(f, &mut app.mgr, &app.mode, &name))?;
                app.sync_sizes();
                let bytes = terminal.backend_mut().take_output();
                if !bytes.is_empty()
                    && write_msg(&mut c.writer, &ServerMsg::Frame(bytes)).await.is_err()
                {
                    client = None;
                    attached = false;
                }
                dirty = false;
            }
        }

        // Persist only when the set of spaces actually changed (no per-event
        // clone of every space name on the hot path).
        if app.mgr.spaces_gen() != last_gen {
            session::save(&name, &app.mgr.space_names());
            last_gen = app.mgr.spaces_gen();
        }

        tokio::select! {
            conn = listener.accept() => {
                if let Ok((stream, _)) = conn {
                    // Read this connection's first message inline (with a timeout)
                    // and decide its fate immediately. Doing it here — rather than
                    // parking it in a shared slot — is essential: liveness probes
                    // (`is_active`) connect and drop constantly, and a shared slot
                    // would let a probe clobber a real Attach/Shutdown before it
                    // was read.
                    let (mut r, w) = stream.into_split();
                    let first = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        read_msg::<_, ClientMsg>(&mut r),
                    ).await;
                    match first {
                        Ok(Ok(ClientMsg::Attach { cols, rows })) => {
                            client = Some(Client { reader: r, writer: w });
                            terminal.backend_mut().set_size(cols, rows);
                            let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, cols, rows));
                            terminal.clear()?;
                            attached = true;
                            dirty = true;
                        }
                        Ok(Ok(ClientMsg::Shutdown)) => shutdown = true,
                        Ok(Ok(ClientMsg::Rename(new))) => rename_to = Some(new),
                        _ => {} // probe, timeout, or junk — discard
                    }
                }
            }
            msg = next_msg(&mut client) => {
                match msg {
                    Ok(ClientMsg::Attach { cols, rows }) => {
                        terminal.backend_mut().set_size(cols, rows);
                        let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, cols, rows));
                        terminal.clear()?;
                        attached = true;
                        dirty = true;
                    }
                    Ok(ClientMsg::Resize { cols, rows }) => {
                        terminal.backend_mut().set_size(cols, rows);
                        let _ = terminal.resize(ratatui::layout::Rect::new(0, 0, cols, rows));
                        terminal.clear()?;
                        dirty = true;
                    }
                    Ok(ClientMsg::Redraw) => {
                        terminal.clear()?;
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
                        attached = false;
                    }
                }
            }
            ev = app.recv_pty() => {
                if let Some(e) = ev {
                    app.drain_pty(e);
                    dirty = true;
                }
            }
        }

        // Apply a rename now that the listener is no longer borrowed.
        if let Some(new) = rename_to.take() {
            let new = session::sanitize(&new);
            if new != name {
                let new_sock = session::socket_path(&new);
                let _ = std::fs::remove_file(&new_sock);
                if let Ok(l) = UnixListener::bind(&new_sock) {
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

    // Session destroyed: tell the client, then remove socket + record.
    if let Some(c) = &mut client {
        let _ = write_msg(&mut c.writer, &ServerMsg::Bye).await;
    }
    session::purge(&name);
    Ok(())
}
