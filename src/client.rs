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
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::protocol::{read_msg, write_msg, ClientMsg, ServerMsg, WireKey};
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
                            let nav = overlay::handle_key(&mut ov, key, &session);
                            match nav {
                                overlay::Nav::Stay => overlay = Some(ov),
                                overlay::Nav::Close => {
                                    // Repaint the underlying session.
                                    write_msg(&mut conn.writer, &ClientMsg::Redraw).await?;
                                }
                                overlay::Nav::Renamed(new) => {
                                    // Current session renamed; the live connection
                                    // is unaffected — just adopt the new name.
                                    session = new;
                                    write_msg(&mut conn.writer, &ClientMsg::Redraw).await?;
                                }
                                overlay::Nav::Switch(target) => {
                                    if target != session {
                                        let _ = write_msg(&mut conn.writer, &ClientMsg::Detach).await;
                                        conn = connect_session(&target, cols, rows).await?;
                                        session = target;
                                    } else {
                                        write_msg(&mut conn.writer, &ClientMsg::Redraw).await?;
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
                        } else {
                            write_msg(&mut conn.writer, &ClientMsg::Key(WireKey::from_event(key))).await?;
                        }
                    }
                    Some(Event::Resize(c, r)) => {
                        cols = c; rows = r;
                        write_msg(&mut conn.writer, &ClientMsg::Resize { cols, rows }).await?;
                        if let Some(ov) = &overlay {
                            term.clear()?;
                            term.draw(|f| overlay::render(f, ov, &session))?;
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
    execute!(stdout, EnterAlternateScreen)?;
    write!(stdout, "\x1b[?25l")?; // hide cursor
    stdout.flush()?;
    Ok(())
}

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b[?25h");
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    let _ = stdout.flush();
    Ok(())
}
