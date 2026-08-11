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
use tokio::sync::mpsc::Sender;

/// How many PTY chunks may sit in flight before the reader threads block.
///
/// This channel is **bounded on purpose**. With an unbounded channel a pane
/// producing output faster than the UI can render it (a build log, `yes`, a
/// `cat` of something huge) grows the queue without limit while the loop is
/// parked writing a frame to a slow client — memory climbs until the machine
/// gives up. Blocking the reader thread instead lets the kernel's PTY buffer
/// fill, which back-pressures the child process: exactly what a terminal is
/// supposed to do.
pub const PTY_QUEUE: usize = 256;

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
    /// The child's pid, which is also its process-group id (`portable-pty` makes
    /// the child a session leader). Kept so we can signal the *whole* group.
    pid: Option<u32>,
}

impl PtyProcess {
    /// Spawn `$SHELL` behind a new PTY sized `cols`x`rows` and start streaming its
    /// output to `tx`, tagged with `id`.
    pub fn spawn(id: u64, cols: u16, rows: u16, tx: Sender<PtyEvent>) -> Result<Self> {
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
        let pid = child.process_id();
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
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(n) => {
                        // Blocking send: applies back-pressure to the child when
                        // the UI can't keep up (see `PTY_QUEUE`).
                        if tx
                            .blocking_send(PtyEvent::Data {
                                id,
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            return; // receiver gone: the session is shutting down
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.blocking_send(PtyEvent::Exited { id });
        });

        Ok(PtyProcess {
            writer,
            master: pair.master,
            child,
            pid,
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

    /// Terminate everything running in the pane and reap the shell.
    ///
    /// Killing only the shell is not enough. Whatever it had running — an
    /// editor, a dev server, an ssh session, a background job — keeps the slave
    /// side of the PTY open, so the master never reaches EOF, the reader thread
    /// parks on a read that never returns, and the fd, the thread and the
    /// process all leak.
    ///
    /// Signalling the shell's process *group* isn't enough either: with job
    /// control on (which it is, because the shell owns a tty) every job bash
    /// starts gets a group of its own. What they do share is the **session**
    /// `portable-pty` created via `setsid`, so that is what we tear down.
    pub fn kill(&mut self) {
        let Some(pid) = self.pid else {
            let _ = self.child.kill();
            let _ = self.child.wait();
            return;
        };
        let sid = pid as libc::pid_t;

        // Ask nicely first: a shell that gets SIGHUP saves its history and hangs
        // up its own jobs on the way out.
        signal_session(sid, libc::SIGHUP);

        // Give it a moment to leave under its own power, then insist.
        for _ in 0..10 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        signal_session(sid, libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send `sig` to every process in the terminal session led by `sid`.
///
/// Linux exposes session membership through `/proc/<pid>/stat`, which is the
/// only portable-in-practice way to reach jobs that moved into their own process
/// group. The session leader itself is signalled through its group so that a
/// job which never registered in `/proc` in time is still covered.
fn signal_session(sid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: `killpg` only interprets a pgid and a signal number; an unknown
    // group returns ESRCH. The leader has not been reaped yet, so its id cannot
    // have been recycled onto an unrelated process.
    unsafe { libc::killpg(sid, sig) };

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
            continue; // not a process directory
        };
        if pid == sid {
            continue; // covered by the killpg above, and reaped by the caller
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // exited between readdir and here
        };
        // `stat` is "pid (comm) state ppid pgrp session ...". `comm` may contain
        // spaces and parentheses, so split after the *last* ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        if rest.split_whitespace().nth(3).and_then(|s| s.parse::<libc::pid_t>().ok()) == Some(sid) {
            // SAFETY: same as above — `kill` on a stale pid returns ESRCH.
            unsafe { libc::kill(pid, sig) };
        }
    }
}

impl Drop for PtyProcess {
    /// Guarantee the shell is killed and reaped whenever a pane goes away —
    /// including when the whole `Manager` is dropped on server shutdown — so no
    /// PTY, file descriptor, reader thread, or child process is ever leaked.
    ///
    /// Order matters: the group dies first, which closes the last slave fd, so
    /// dropping `writer`/`master` afterwards lets the reader thread see EOF and
    /// exit instead of blocking on a read that never returns.
    fn drop(&mut self) {
        self.kill();
    }
}
