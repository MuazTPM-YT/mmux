//! A minimal, fast in-memory terminal emulator.
//!
//! Each pane owns one [`Screen`]. Bytes read from the pane's PTY are fed through
//! a `vte::Parser` whose `Perform` callbacks mutate this grid. The renderer then
//! blits the grid straight into ratatui's back buffer, so `Screen` is the bridge
//! between "raw shell output" and "something drawable".
//!
//! The cell grid is a single contiguous `Vec<Cell>` (row-major) rather than a
//! `Vec<Vec<Cell>>`: one allocation instead of one-per-row, better cache
//! locality when blitting, and scrolling becomes a `copy_within` memmove.

use ratatui::style::Color;
use vte::{Params, Perform};

/// A single character cell with its rendered attributes. `Copy` so fills and
/// scrolls are trivial `memcpy`s.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Reset,
            bg: Color::Reset,
            bold: false,
            dim: false,
            underline: false,
            reverse: false,
        }
    }
}

pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Row-major cell buffer of length `cols * rows`.
    grid: Vec<Cell>,
    pub cx: u16,
    pub cy: u16,
    /// Optional window title captured from OSC 0/2 (shown in the pane border).
    pub title: Option<String>,
    /// Bytes the emulator owes back to the shell (answers to DA/DSR queries).
    /// The owning `Pane` drains this after each parse and writes it to the PTY;
    /// leaving these unanswered makes shells like fish stall for ~10s on startup.
    pub responses: Vec<u8>,

    // Pending pen attributes applied to newly printed cells.
    pen: Cell,
    // Deferred line-wrap: set once the cursor is pushed past the last column so
    // that a trailing character at the edge does not eagerly scroll.
    wrap_pending: bool,
    // Saved cursor (DECSC / DECRC).
    saved: (u16, u16),
    // Scroll region (inclusive), defaults to the whole screen.
    scroll_top: u16,
    scroll_bot: u16,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            grid: vec![Cell::default(); cols as usize * rows as usize],
            cx: 0,
            cy: 0,
            title: None,
            responses: Vec::new(),
            pen: Cell::default(),
            wrap_pending: false,
            saved: (0, 0),
            scroll_top: 0,
            scroll_bot: rows - 1,
        }
    }

    #[inline]
    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.cols as usize + x as usize
    }

    /// Read a cell for rendering. `None` if out of bounds.
    #[inline]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x < self.cols && y < self.rows {
            Some(&self.grid[self.idx(x, y)])
        } else {
            None
        }
    }

    /// Resize the grid, preserving as much content as possible (top-anchored).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut new_grid = vec![Cell::default(); cols as usize * rows as usize];
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for y in 0..copy_rows {
            let src = self.idx(0, y);
            let dst = y as usize * cols as usize;
            new_grid[dst..dst + copy_cols as usize]
                .copy_from_slice(&self.grid[src..src + copy_cols as usize]);
        }
        self.grid = new_grid;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.wrap_pending = false;
    }

    #[inline]
    fn blank(&self) -> Cell {
        // Blank cells keep the current background so that reverse-video fills work.
        Cell {
            bg: self.pen.bg,
            ..Cell::default()
        }
    }

    /// Scroll the region [scroll_top, scroll_bot] up by `n` rows (content moves
    /// up, blank rows appear at the bottom).
    fn scroll_up(&mut self, n: u16) {
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let bot = self.scroll_bot as usize;
        let region_rows = bot + 1 - top;
        let n = (n as usize).min(region_rows);
        if n == 0 {
            return;
        }
        let start = top * cols;
        let end = (bot + 1) * cols;
        let shift = n * cols;
        self.grid.copy_within(start + shift..end, start);
        let blank = self.blank();
        self.grid[end - shift..end].fill(blank);
    }

    /// Scroll the region down by `n` rows (blank rows appear at the top).
    fn scroll_down(&mut self, n: u16) {
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let bot = self.scroll_bot as usize;
        let region_rows = bot + 1 - top;
        let n = (n as usize).min(region_rows);
        if n == 0 {
            return;
        }
        let start = top * cols;
        let end = (bot + 1) * cols;
        let shift = n * cols;
        self.grid.copy_within(start..end - shift, start + shift);
        let blank = self.blank();
        self.grid[start..start + shift].fill(blank);
    }

    fn line_feed(&mut self) {
        if self.cy == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn put(&mut self, c: char) {
        if self.wrap_pending {
            self.cx = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        let idx = self.idx(self.cx, self.cy);
        if idx < self.grid.len() {
            let mut cell = self.pen;
            cell.ch = c;
            self.grid[idx] = cell;
        }
        if self.cx + 1 >= self.cols {
            // Stay on the last column; wrap only when the next glyph arrives.
            self.wrap_pending = true;
        } else {
            self.cx += 1;
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let (start, end) = match mode {
            0 => (self.cx, self.cols),      // cursor -> EOL
            1 => (0, self.cx + 1),          // BOL -> cursor
            2 => (0, self.cols),            // whole line
            _ => return,
        };
        let blank = self.blank();
        let row = self.cy as usize * self.cols as usize;
        let end = end.min(self.cols) as usize;
        self.grid[row + start as usize..row + end].fill(blank);
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        let cols = self.cols as usize;
        match mode {
            0 => {
                self.erase_in_line(0);
                let from = (self.cy as usize + 1) * cols;
                self.grid[from..].fill(blank);
            }
            1 => {
                self.erase_in_line(1);
                let to = self.cy as usize * cols;
                self.grid[..to].fill(blank);
            }
            2 | 3 => self.grid.fill(blank),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        // No params == reset (ESC[m).
        if params.is_empty() {
            self.pen = Cell::default();
            return;
        }
        let mut iter = params.iter();
        while let Some(p) = iter.next() {
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Cell::default(),
                1 => self.pen.bold = true,
                2 => self.pen.dim = true,
                4 => self.pen.underline = true,
                7 => self.pen.reverse = true,
                22 => {
                    // Normal intensity: clears both bold and faint.
                    self.pen.bold = false;
                    self.pen.dim = false;
                }
                24 => self.pen.underline = false,
                27 => self.pen.reverse = false,
                30..=37 => self.pen.fg = ansi_color(code - 30),
                39 => self.pen.fg = Color::Reset,
                40..=47 => self.pen.bg = ansi_color(code - 40),
                49 => self.pen.bg = Color::Reset,
                90..=97 => self.pen.fg = ansi_bright(code - 90),
                100..=107 => self.pen.bg = ansi_bright(code - 100),
                38 | 48 => {
                    // Extended color: 38;5;n (256) or 38;2;r;g;b (truecolor).
                    let is_fg = code == 38;
                    match iter.next().and_then(|s| s.first().copied()) {
                        Some(5) => {
                            if let Some(n) = iter.next().and_then(|s| s.first().copied()) {
                                let c = Color::Indexed(n as u8);
                                if is_fg { self.pen.fg = c } else { self.pen.bg = c }
                            }
                        }
                        Some(2) => {
                            let r = iter.next().and_then(|s| s.first().copied()).unwrap_or(0) as u8;
                            let g = iter.next().and_then(|s| s.first().copied()).unwrap_or(0) as u8;
                            let b = iter.next().and_then(|s| s.first().copied()).unwrap_or(0) as u8;
                            let c = Color::Rgb(r, g, b);
                            if is_fg { self.pen.fg = c } else { self.pen.bg = c }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn ansi_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

fn ansi_bright(n: u16) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        7 => Color::White,
        _ => Color::Reset,
    }
}

/// The `idx`-th CSI parameter, defaulting to `default` when absent/zero-omitted.
fn arg(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|s| s.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                // Backspace
                if self.cx > 0 {
                    self.cx -= 1;
                }
                self.wrap_pending = false;
            }
            0x09 => {
                // Tab to next multiple of 8
                let next = ((self.cx / 8) + 1) * 8;
                self.cx = next.min(self.cols - 1);
                self.wrap_pending = false;
            }
            0x0A..=0x0C => {
                self.line_feed();
                self.wrap_pending = false;
            }
            0x0D => {
                self.cx = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Device queries may carry a '>' / '?' marker — handle them first.
        match action {
            'c' => {
                if intermediates.first() == Some(&b'>') {
                    // Secondary DA (DA2): terminal-type ; version ; 0.
                    self.responses.extend_from_slice(b"\x1b[>0;95;0c");
                } else {
                    // Primary DA (DA1): advertise VT220 + ANSI colour (22).
                    self.responses.extend_from_slice(b"\x1b[?62;22c");
                }
                return;
            }
            'n' => {
                let private = intermediates.first() == Some(&b'?');
                match arg(params, 0, 0) {
                    5 if !private => self.responses.extend_from_slice(b"\x1b[0n"),
                    6 => {
                        // Cursor Position Report, 1-based; echo the '?' for DECXCPR.
                        let prefix: &[u8] = if private { b"\x1b[?" } else { b"\x1b[" };
                        self.responses.extend_from_slice(prefix);
                        self.responses
                            .extend_from_slice(format!("{};{}R", self.cy + 1, self.cx + 1).as_bytes());
                    }
                    _ => {}
                }
                return;
            }
            _ => {}
        }

        // Any other CSI carrying an intermediate / private marker — e.g.
        // modifyOtherKeys `ESC[>4;2m`, Kitty keyboard `ESC[>1u`, DEC private
        // modes `ESC[?…h` — is NOT a standard SGR/cursor op. Ignoring it is
        // essential: otherwise `ESC[>4;2m` gets misparsed as SGR "4" (underline),
        // which sticks underline on for everything after it.
        if !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => self.cy = self.cy.saturating_sub(arg(params, 0, 1)),
            'B' => self.cy = (self.cy + arg(params, 0, 1)).min(self.rows - 1),
            'C' => {
                self.cx = (self.cx + arg(params, 0, 1)).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'D' => {
                self.cx = self.cx.saturating_sub(arg(params, 0, 1));
                self.wrap_pending = false;
            }
            'G' => self.cx = (arg(params, 0, 1) - 1).min(self.cols - 1),
            'd' => self.cy = (arg(params, 0, 1) - 1).min(self.rows - 1),
            'H' | 'f' => {
                self.cy = (arg(params, 0, 1) - 1).min(self.rows - 1);
                self.cx = (arg(params, 1, 1) - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'J' => self.erase_in_display(arg(params, 0, 0)),
            'K' => self.erase_in_line(arg(params, 0, 0)),
            'L' => self.scroll_down(arg(params, 0, 1)),
            'M' => self.scroll_up(arg(params, 0, 1)),
            'r' => {
                self.scroll_top = (arg(params, 0, 1) - 1).min(self.rows - 1);
                self.scroll_bot = (arg(params, 1, self.rows) - 1).min(self.rows - 1);
                self.cx = 0;
                self.cy = 0;
            }
            'm' => self.apply_sgr(params),
            's' => self.saved = (self.cx, self.cy),
            'u' => {
                self.cx = self.saved.0;
                self.cy = self.saved.1;
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => {
                // Reverse index: move up, scrolling the region if at the top.
                if self.cy == self.scroll_top {
                    self.scroll_down(1);
                } else if self.cy > 0 {
                    self.cy -= 1;
                }
            }
            b'7' => self.saved = (self.cx, self.cy),
            b'8' => {
                self.cx = self.saved.0;
                self.cy = self.saved.1;
            }
            b'c' => {
                // Full reset
                let (c, r) = (self.cols, self.rows);
                *self = Screen::new(c, r);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 2 set the window/icon title.
        if let (Some(kind), Some(text)) = (params.first(), params.get(1)) {
            if *kind == b"0" || *kind == b"2" {
                self.title = Some(String::from_utf8_lossy(text).into_owned());
            }
        }
    }

    // Unused DCS hooks.
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    /// Feed raw bytes through a fresh parser into a screen of the given size.
    fn render(cols: u16, rows: u16, input: &[u8]) -> Screen {
        let mut screen = Screen::new(cols, rows);
        let mut parser = Parser::new();
        for &b in input {
            parser.advance(&mut screen, b);
        }
        screen
    }

    fn row_text(screen: &Screen, y: u16) -> String {
        (0..screen.cols)
            .map(|x| screen.cell(x, y).unwrap().ch)
            .collect()
    }

    #[test]
    fn prints_plain_text() {
        let s = render(20, 5, b"hello");
        assert_eq!(row_text(&s, 0).trim_end(), "hello");
        assert_eq!(s.cx, 5);
    }

    #[test]
    fn newline_and_carriage_return() {
        let s = render(20, 5, b"ab\r\ncd");
        assert_eq!(row_text(&s, 0).trim_end(), "ab");
        assert_eq!(row_text(&s, 1).trim_end(), "cd");
        assert_eq!((s.cx, s.cy), (2, 1));
    }

    #[test]
    fn cursor_position_and_sgr_color() {
        // Move to row 2 col 3, then print red 'X'.
        let s = render(20, 5, b"\x1b[2;3H\x1b[31mX");
        assert_eq!(s.cell(2, 1).unwrap().ch, 'X');
        assert_eq!(s.cell(2, 1).unwrap().fg, Color::Red);
    }

    #[test]
    fn erase_in_line_clears_to_end() {
        let s = render(20, 5, b"abcdef\x1b[4D\x1b[K");
        // Cursor moved back 4 (to col 2), erase-to-EOL wipes "cdef".
        assert_eq!(row_text(&s, 0).trim_end(), "ab");
    }

    #[test]
    fn scrolls_when_output_exceeds_height() {
        let s = render(10, 3, b"1\r\n2\r\n3\r\n4");
        // First line scrolled off the top.
        assert_eq!(row_text(&s, 0).trim_end(), "2");
        assert_eq!(row_text(&s, 2).trim_end(), "4");
    }

    #[test]
    fn osc_sets_title() {
        let s = render(20, 5, b"\x1b]2;my title\x07");
        assert_eq!(s.title.as_deref(), Some("my title"));
    }

    #[test]
    fn line_wraps_at_edge() {
        // Width 3: "abcd" -> "abc" on row 0, "d" on row 1.
        let s = render(3, 3, b"abcd");
        assert_eq!(row_text(&s, 0), "abc");
        assert_eq!(row_text(&s, 1).trim_end(), "d");
    }

    #[test]
    fn resize_preserves_content() {
        let mut s = render(10, 4, b"row0\r\nrow1");
        s.resize(20, 6);
        assert_eq!(row_text(&s, 0).trim_end(), "row0");
        assert_eq!(row_text(&s, 1).trim_end(), "row1");
        assert_eq!((s.cols, s.rows), (20, 6));
    }

    #[test]
    fn answers_primary_device_attributes() {
        // This is the query fish blocks on for 10s if left unanswered.
        let s = render(20, 5, b"\x1b[c");
        assert_eq!(s.responses, b"\x1b[?62;22c");
    }

    #[test]
    fn answers_primary_da_with_zero_param() {
        let s = render(20, 5, b"\x1b[0c");
        assert_eq!(s.responses, b"\x1b[?62;22c");
    }

    #[test]
    fn answers_secondary_device_attributes() {
        let s = render(20, 5, b"\x1b[>c");
        assert_eq!(s.responses, b"\x1b[>0;95;0c");
    }

    #[test]
    fn answers_cursor_position_report() {
        // Move to row 3, col 5, then request the position (DSR 6).
        let s = render(20, 5, b"\x1b[3;5H\x1b[6n");
        assert_eq!(s.responses, b"\x1b[3;5R");
    }

    #[test]
    fn answers_device_status_ok() {
        let s = render(20, 5, b"\x1b[5n");
        assert_eq!(s.responses, b"\x1b[0n");
    }

    /// Regression: `ESC[>4;2m` is XTMODKEYS (modifyOtherKeys), NOT an SGR. It
    /// must not be parsed as SGR "4" (underline) — that was the bug that made
    /// Claude Code render everything underlined.
    #[test]
    fn private_marker_csi_m_is_not_sgr() {
        let s = render(20, 5, b"\x1b[>4;2mX");
        let cell = s.cell(0, 0).unwrap();
        assert!(!cell.underline, "private CSI must not set underline");
        assert!(!cell.dim);
        assert!(!s.pen.underline);
    }

    /// `ESC[>1u` is a Kitty-keyboard push, not DECRC (cursor restore).
    #[test]
    fn private_marker_csi_u_is_not_cursor_restore() {
        // Save cursor at (0,0), move away, then send the Kitty sequence: the
        // cursor must NOT jump back to the saved position.
        let s = render(20, 5, b"\x1b[s\x1b[3;5H\x1b[>1u");
        assert_eq!((s.cx, s.cy), (4, 2));
    }

    #[test]
    fn dim_attribute_is_tracked() {
        let s = render(20, 5, b"\x1b[2mX\x1b[22mY");
        assert!(s.cell(0, 0).unwrap().dim);
        assert!(!s.cell(1, 0).unwrap().dim);
    }
}
