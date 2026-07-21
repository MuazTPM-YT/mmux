//! PTY lifecycle: spawn a shell behind a pseudo-terminal and pump its output
//! into the async event loop.
//!
//! `portable-pty` exposes blocking `std::io` readers, so each pane gets a
//! dedicated OS thread that reads bytes and forwards them over a Tokio channel.
//! This keeps the single async loop from ever blocking on a slow child.

use std::io::{Read, Write};
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc::UnboundedSender;

/// Output routed from a PTY reader thread to the main loop.
pub enum PtyEvent {
    /// A chunk of bytes from pane `id`.
    Data { id: u64, bytes: Vec<u8> },
    /// The child process for pane `id` exited / the PTY closed.
    Exited { id: u64 },
}

/// The writable/controllable side of a spawned pane, owned by [`crate::state::Pane`].
pub struct PtyProcess {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtyProcess {
    /// Spawn `$SHELL` behind a new PTY sized `cols`x`rows` and start streaming its
    /// output to `tx`, tagged with `id`.
    pub fn spawn(id: u64, cols: u16, rows: u16, tx: UnboundedSender<PtyEvent>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // The slave handle is no longer needed once the child owns it; dropping it
        // ensures EOF propagates correctly when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .send(PtyEvent::Data {
                                id,
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break; // receiver gone: app is shutting down
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(PtyEvent::Exited { id });
        });

        Ok(PtyProcess {
            writer,
            master: pair.master,
            child,
        })
    }

    /// Forward raw input bytes to the shell.
    pub fn write(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    /// Inform the child of a new pane size (triggers SIGWINCH).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Terminate the child process and reap it, so no zombie is left behind.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtyProcess {
    /// Guarantee the shell is killed and reaped whenever a pane goes away —
    /// including when the whole `Manager` is dropped on server shutdown — so no
    /// PTY, file descriptor, or child process is ever leaked.
    fn drop(&mut self) {
        self.kill();
    }
}
