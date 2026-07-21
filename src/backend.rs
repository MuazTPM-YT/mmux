//! A ratatui `Backend` for the headless server.
//!
//! The server has no real terminal, so it can't use `CrosstermBackend` directly
//! (that queries the tty for its size). `VirtualBackend` delegates all drawing to
//! a `CrosstermBackend` writing into an in-memory buffer, but reports a
//! **fixed size** (the attached client's) and never queries a tty. The captured
//! escape bytes are shipped to the client, which writes them to its real terminal.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ratatui::backend::{Backend, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// A `Write` sink shared with `VirtualBackend` so we can drain what was rendered.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn take(&self) -> Vec<u8> {
        // Recover from a poisoned lock rather than panicking — the buffer is
        // just bytes, so a previous panic can't have left it inconsistent.
        std::mem::take(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct VirtualBackend {
    inner: CrosstermBackend<SharedBuf>,
    buf: SharedBuf,
    size: Size,
}

impl VirtualBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        let buf = SharedBuf::default();
        VirtualBackend {
            inner: CrosstermBackend::new(buf.clone()),
            buf,
            size: Size::new(cols.max(1), rows.max(1)),
        }
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.size = Size::new(cols.max(1), rows.max(1));
    }

    /// Drain and return everything rendered since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        self.buf.take()
    }
}

impl Backend for VirtualBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        // Never query a (non-existent) tty.
        Ok(Position::new(0, 0))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
