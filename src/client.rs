//! The **client** (`mmux <name>`): owns the real terminal, attaches to a
//! session's server (spawning it if needed), pipes rendered frames to the
//! screen, and forwards input.
//!
//! Global chords the client intercepts (never forwarded to the session):
//!   * `Alt+Enter` — detach (the server keeps the shells alive)
//!   * `Alt+.`     — session list overlay (navigate, `Enter` switch, `D`/`R`/`N`)
//!   * `Alt+/`     — session search overlay (type to filter, `Enter` switch)

use std::io::{self, Stdout, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::protocol::{read_msg, write_msg, ClientMsg, ServerMsg, WireKey, WireMouse};
use crate::session;

mod overlay;
use overlay::Overlay;

/// Ensure a server is listening for `name`, spawning a detached daemon if not.
fn ensure_server(name: &str) -> Result<()> {
    if session::is_active(name) {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__server")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // setsid: detach from the controlling terminal so the daemon outlives this
    // client and survives the terminal closing.
    //
    // SAFETY: `pre_exec` runs in the forked child before `exec`. The closure only
    // calls `setsid`, which is async-signal-safe, and allocates nothing — the
    // sole requirements for code running between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;

    for _ in 0..150 {
        if session::is_active(name) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(anyhow!("mmux server for '{name}' did not start"))
}

struct Conn {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

/// Ensure `name`'s server exists, connect, and attach at the given size.
async fn connect_session(name: &str, cols: u16, rows: u16) -> Result<Conn> {
    let owned = name.to_string();
    tokio::task::spawn_blocking(move || ensure_server(&owned)).await??;
    let stream = UnixStream::connect(session::socket_path(name)).await?;
    let (reader, mut writer) = stream.into_split();
    write_msg(&mut writer, &ClientMsg::Attach { cols, rows }).await?;
    Ok(Conn { reader, writer })
}

pub async fn run(name: String) -> Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));
    setup_terminal()?;
    let result = drive(name).await;
    restore_terminal()?;
    result
}

async fn drive(mut session: String) -> Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (mut cols, mut rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut conn = connect_session(&session, cols, rows).await?;

    // Read terminal events on a dedicated blocking thread and funnel them into
    // the async loop. This avoids pulling in `futures` / crossterm's EventStream.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if input_tx.send(ev).is_err() {
                break; // the client is shutting down
            }
        }
    });

    let mut out = io::stdout();
    let mut overlay: Option<Overlay> = None;

    loop {
        tokio::select! {
            ev = input_rx.recv() => {
                match ev {
                    Some(Event::Key(key)) => {
                        if key.kind == KeyEventKind::Release { continue; }
                        if let Some(mut ov) = overlay.take() {
                            // The switcher probes every session socket and can
                            // block for up to a second waiting on a rename or
                            // shutdown. `block_in_place` lets tokio move the rest
                            // of the runtime off this worker instead of stalling
                            // it, so frames keep draining while we wait.
                            let nav = tokio::task::block_in_place(|| {
                                overlay::handle_key(&mut ov, key, &session)
                            });
                            match nav {
                                overlay::Nav::Stay => overlay = Some(ov),
                                overlay::Nav::Close => {
                                    // Repaint the underlying session.
                                    if write_msg(&mut conn.writer, &ClientMsg::Redraw).await.is_err() {
                                        break;
                                    }
                                }
                                overlay::Nav::Renamed(new) => {
                                    // Current session renamed; the live connection
                                    // is unaffected — just adopt the new name.
                                    session = new;
                                    if write_msg(&mut conn.writer, &ClientMsg::Redraw).await.is_err() {
                                        break;
                                    }
                                }
                                overlay::Nav::Switch(target) => {
                                    if target != session {
                                        let _ = write_msg(&mut conn.writer, &ClientMsg::Detach).await;
                                        conn = connect_session(&target, cols, rows).await?;
                                        session = target;
                                    } else if write_msg(&mut conn.writer, &ClientMsg::Redraw).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            if let Some(ov) = &overlay {
                                term.draw(|f| overlay::render(f, ov, &session))?;
                            }
                        } else if is_detach(&key) {
                            let _ = write_msg(&mut conn.writer, &ClientMsg::Detach).await;
                            break;
                        } else if is_chord(&key, '.') {
                            let ov = Overlay::list(&session);
                            term.clear()?;
                            term.draw(|f| overlay::render(f, &ov, &session))?;
                            overlay = Some(ov);
                        } else if is_chord(&key, '/') {
                            let ov = Overlay::search(&session);
                            term.clear()?;
                            term.draw(|f| overlay::render(f, &ov, &session))?;
                            overlay = Some(ov);
                        } else if write_msg(&mut conn.writer, &ClientMsg::Key(WireKey::from_event(key)))
                            .await
                            .is_err()
                        {
                            break; // server gone — leave cleanly, not with an error
                        }
                    }
                    Some(Event::Resize(c, r)) => {
                        cols = c; rows = r;
                        if write_msg(&mut conn.writer, &ClientMsg::Resize { cols, rows }).await.is_err() {
                            break;
                        }
                        if let Some(ov) = &overlay {
                            term.clear()?;
                            term.draw(|f| overlay::render(f, ov, &session))?;
                        }
                    }
                    // The switcher owns the screen while it is up, so mouse
                    // events there would act on a layout the user can't see.
                    Some(Event::Mouse(m)) if overlay.is_none() => {
                        if write_msg(&mut conn.writer, &ClientMsg::Mouse(WireMouse::from_event(m)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Event::Paste(text)) => {
                        // Ignore pastes while an overlay owns the screen; otherwise
                        // forward to the session, which routes it to the focused pane.
                        if overlay.is_none()
                            && write_msg(&mut conn.writer, &ClientMsg::Paste(text)).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            msg = read_msg::<_, ServerMsg>(&mut conn.reader) => {
                match msg {
                    Ok(ServerMsg::Frame(bytes)) => {
                        // While an overlay is up the client owns the screen.
                        if overlay.is_none() {
                            out.write_all(&bytes)?;
                            out.flush()?;
                        }
                    }
                    Ok(ServerMsg::Bye) | Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

/// `Alt+Enter` detaches.
fn is_detach(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT)
}

/// `Alt+<c>` (with Shift possibly consumed into the glyph).
fn is_chord(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char(c)
}

fn setup_terminal() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout: Stdout = io::stdout();
    // EnableBracketedPaste (DECSET 2004) tells the outer terminal that mmux
    // handles pastes safely — without it, terminals like Ghostty warn on every
    // paste containing control codes, because unbracketed pastes are injected as
    // raw keystrokes. It also makes crossterm coalesce pastes into `Event::Paste`.
    //
    // EnableMouseCapture routes clicks and the wheel to mmux, which focuses
    // panes, drags split seams, and scrolls scrollback — forwarding the event to
    // the pane's app when that app asked for tracking. Holding Shift bypasses
    // reporting in every terminal, so the outer terminal's own select-and-copy
    // keeps working; that is why mmux does not implement its own selection.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // Ask the outer terminal to disambiguate escape codes (Kitty keyboard
    // protocol). Without this, terminals fold Shift+Enter into a bare `\r`, so
    // the modifier never reaches mmux and apps like Claude Code can't tell the
    // two apart. Harmless on terminals that don't support it — they ignore it.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    write!(stdout, "\x1b[?25l")?; // hide cursor
    stdout.flush()?;
    Ok(())
}

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b[?25h");
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, DisableBracketedPaste);
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    let _ = stdout.flush();
    Ok(())
}
