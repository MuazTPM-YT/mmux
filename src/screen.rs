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
//!
//! Correctness rules that are load-bearing here:
//!
//! * **Never leave a query unanswered.** DA1/DA2 (`CSI c`), DSR (`CSI n`) and
//!   window reports (`CSI t`) are queries the shell *blocks on* — fish waits
//!   ~10s for the DA1 reply before drawing its prompt. Replies are pushed into
//!   [`Screen::responses`], which `Pane::feed` drains back to the PTY.
//! * **CSI intermediates matter.** `csi_dispatch` answers queries first, then
//!   returns early for any CSI carrying an intermediate/private marker (`>`,
//!   `?`, `!`, …). `ESC[>4;2m` (modifyOtherKeys) and `ESC[>1u` (Kitty keyboard)
//!   share final bytes with SGR (`m`) and DECRC (`u`); without the guard
//!   `>4;2m` sets SGR "4" (underline) and sticks it on everything after it.
//! * **One char per cell, but cells have a width.** A double-width glyph (CJK,
//!   emoji) occupies its own cell plus a zero-width *continuation* cell, so the
//!   grid stays column-aligned with what the real terminal will draw.

use ratatui::style::Color;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// Longest window title we keep, so a rogue OSC can't grow a pane without bound.
const MAX_TITLE: usize = 256;
/// Cap on unsent query replies. Replies are drained after every `Pane::feed`, so
/// this only bounds a pathological burst inside one chunk.
const MAX_RESPONSES: usize = 64 * 1024;
/// Kitty's keyboard-protocol flag stack is explicitly bounded by the spec.
const MAX_KITTY_STACK: usize = 16;

/// A single character cell with its rendered attributes. `Copy` so fills and
/// scrolls are trivial `memcpy`s.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub reverse: bool,
    pub strike: bool,
    /// Display width: `1` normal, `2` the left half of a double-width glyph,
    /// `0` the right half (a continuation cell that is never drawn on its own).
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Reset,
            bg: Color::Reset,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            blink: false,
            reverse: false,
            strike: false,
            width: 1,
        }
    }
}

/// Which glyph table `print` maps through. `ESC ( 0` selects DEC Special
/// Graphics — this is how ncurses draws boxes on `TERM=xterm-256color`, so
/// without it `tput`/`dialog`/older TUIs render as literal `qqqlkmjx` soup.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Charset {
    #[default]
    Ascii,
    DecSpecial,
}

/// The primary screen, parked while the alternate screen is active.
struct SavedScreen {
    grid: Vec<Cell>,
    cx: u16,
    cy: u16,
    pen: Cell,
    scroll_top: u16,
    scroll_bot: u16,
    saved_cursor: SavedCursor,
}

#[derive(Clone, Copy, Default)]
struct SavedCursor {
    cx: u16,
    cy: u16,
    pen: Cell,
    origin_mode: bool,
    charsets: [Charset; 2],
    active_charset: usize,
}

pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Row-major cell buffer of length `cols * rows`.
    grid: Vec<Cell>,
    pub cx: u16,
    pub cy: u16,
    /// Optional window title captured from OSC 0/2 (available to the UI).
    pub title: Option<String>,
    /// Bytes the emulator owes back to the shell (answers to DA/DSR/window
    /// queries). The owning `Pane` drains this after each parse and writes it to
    /// the PTY; leaving these unanswered makes shells like fish stall for ~10s.
    pub responses: Vec<u8>,
    /// Whether the pane's app enabled bracketed-paste mode (DECSET 2004). When
    /// set, pasted text forwarded to this pane is wrapped in `ESC[200~`/`ESC[201~`
    /// so the shell treats it as literal data instead of executable keystrokes.
    pub bracketed_paste: bool,
    /// DECTCEM (`?25`): whether the app wants a visible cursor. Full-screen apps
    /// hide it while repainting; drawing it anyway leaves a block skidding around
    /// the pane.
    pub cursor_visible: bool,
    /// DECCKM (`?1`): application cursor-key mode. When set, arrows must be sent
    /// as SS3 (`ESC O A`) rather than CSI (`ESC [ A`) — see `keys::encode_key`.
    pub app_cursor_keys: bool,
    /// Current Kitty keyboard-protocol flags for this pane (0 = disabled). Apps
    /// like Claude Code enable it (`CSI > flags u`) to receive keys that legacy
    /// encodings can't express — notably Shift+Enter.
    pub kitty_keyboard: u8,
    /// Saved-flag stack backing the protocol's push (`>`) / pop (`<`) operations.
    kitty_stack: Vec<u8>,

    // Pending pen attributes applied to newly printed cells.
    pen: Cell,
    // Deferred line-wrap: set once the cursor is pushed past the last column so
    // that a trailing character at the edge does not eagerly scroll.
    wrap_pending: bool,
    // DECAWM (`?7`): auto-wrap at the right margin. Apps turn this off to paint
    // the last column without scrolling.
    autowrap: bool,
    // DECOM (`?6`): cursor addressing is relative to the scroll region.
    origin_mode: bool,
    // IRM (`CSI 4 h`): printed characters shift the rest of the line right.
    insert_mode: bool,
    // Saved cursor (DECSC / DECRC / `CSI s` / `CSI u`).
    saved_cursor: SavedCursor,
    // Scroll region (inclusive), defaults to the whole screen.
    scroll_top: u16,
    scroll_bot: u16,
    // G0/G1 glyph tables and which one is currently mapped into GL (SI/SO).
    charsets: [Charset; 2],
    active_charset: usize,
    // Horizontal tab stops, one flag per column (HTS / TBC).
    tabs: Vec<bool>,
    // Last printed character, for REP (`CSI b`).
    last_char: char,
    // Primary screen contents while the alternate screen is active.
    alt: Option<Box<SavedScreen>>,
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
            bracketed_paste: false,
            cursor_visible: true,
            app_cursor_keys: false,
            kitty_keyboard: 0,
            kitty_stack: Vec::new(),
            pen: Cell::default(),
            wrap_pending: false,
            autowrap: true,
            origin_mode: false,
            insert_mode: false,
            saved_cursor: SavedCursor::default(),
            scroll_top: 0,
            scroll_bot: rows - 1,
            charsets: [Charset::Ascii; 2],
            active_charset: 0,
            tabs: default_tabs(cols),
            last_char: ' ',
            alt: None,
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

    /// Resize the grid, preserving as much content as possible.
    ///
    /// Growing keeps the content top-anchored. **Shrinking drops rows from the
    /// top**, not the bottom: the interesting part of a terminal is the cursor
    /// and the lines around it, so bottom-anchoring is what keeps your prompt on
    /// screen when a window gets shorter.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // How many rows scroll off the top when shrinking — exactly enough to
        // keep the cursor row on screen.
        let drop_top = if self.cy >= rows { self.cy + 1 - rows } else { 0 };
        let copy_rows = rows.min(self.rows - drop_top);
        let copy_cols = cols.min(self.cols);

        let mut new_grid = vec![Cell::default(); cols as usize * rows as usize];
        for y in 0..copy_rows {
            let src = self.idx(0, y + drop_top);
            let dst = y as usize * cols as usize;
            new_grid[dst..dst + copy_cols as usize]
                .copy_from_slice(&self.grid[src..src + copy_cols as usize]);
        }
        // Resize the parked primary screen too, or switching back reveals a grid
        // of the wrong shape.
        if let Some(alt) = self.alt.as_mut() {
            alt.grid = regrid(&alt.grid, self.cols, self.rows, cols, rows);
            alt.cx = alt.cx.min(cols - 1);
            alt.cy = alt.cy.min(rows - 1);
            alt.scroll_top = 0;
            alt.scroll_bot = rows - 1;
        }

        self.grid = new_grid;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.saturating_sub(drop_top).min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.wrap_pending = false;
        self.tabs = default_tabs(cols);
        // A narrower grid can slice a double-width glyph in half, leaving a lead
        // cell in the last column with nowhere to put its continuation.
        for y in 0..rows {
            self.heal_row_tail(y);
        }
    }

    #[inline]
    fn blank(&self) -> Cell {
        // Blank cells keep the current background so that background-colour
        // erase (BCE) works the way apps expect.
        Cell {
            bg: self.pen.bg,
            ..Cell::default()
        }
    }

    // ---- double-width bookkeeping ----------------------------------------
    //
    // A wide glyph lives in two cells: the lead (`width == 2`) and a
    // continuation (`width == 0`). Any edit that touches one half must clear the
    // other, otherwise the renderer emits a two-column glyph over a cell that
    // now holds something else and the whole line shifts.

    /// Blank the partner half of whichever double-width glyph covers `x`.
    fn clear_wide_partner(&mut self, x: u16, y: u16) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let blank = self.blank();
        let i = self.idx(x, y);
        match self.grid[i].width {
            0 if x > 0 => {
                let p = self.idx(x - 1, y);
                self.grid[p] = blank;
            }
            2 if x + 1 < self.cols => {
                let p = self.idx(x + 1, y);
                self.grid[p] = blank;
            }
            _ => {}
        }
    }

    /// After overwriting `[start, end)` on row `y`, blank any lone half left
    /// dangling just outside the span.
    fn heal_span(&mut self, y: u16, start: u16, end: u16) {
        if y >= self.rows {
            return;
        }
        let blank = self.blank();
        if start > 0 && start <= self.cols {
            let i = self.idx(start - 1, y);
            if self.grid[i].width == 2 {
                self.grid[i] = blank;
            }
        }
        if end < self.cols {
            let i = self.idx(end, y);
            if self.grid[i].width == 0 {
                self.grid[i] = blank;
            }
        }
    }

    /// A lead half in the last column has nowhere to put its continuation.
    fn heal_row_tail(&mut self, y: u16) {
        if y >= self.rows {
            return;
        }
        let blank = self.blank();
        let i = self.idx(self.cols - 1, y);
        if self.grid[i].width == 2 {
            self.grid[i] = blank;
        }
    }

    // ---- scrolling --------------------------------------------------------

    /// Scroll rows `[top, bot]` up by `n` (content moves up, blanks at the bottom).
    fn scroll_region_up(&mut self, top: u16, bot: u16, n: u16) {
        if top > bot || bot >= self.rows {
            return;
        }
        let cols = self.cols as usize;
        let (top, bot) = (top as usize, bot as usize);
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

    /// Scroll rows `[top, bot]` down by `n` (blanks appear at the top).
    fn scroll_region_down(&mut self, top: u16, bot: u16, n: u16) {
        if top > bot || bot >= self.rows {
            return;
        }
        let cols = self.cols as usize;
        let (top, bot) = (top as usize, bot as usize);
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

    fn scroll_up(&mut self, n: u16) {
        self.scroll_region_up(self.scroll_top, self.scroll_bot, n);
    }

    fn scroll_down(&mut self, n: u16) {
        self.scroll_region_down(self.scroll_top, self.scroll_bot, n);
    }

    /// IL — insert `n` blank lines at the cursor, pushing the rest of the scroll
    /// region down. Note the region starts at the **cursor row**, not at
    /// `scroll_top`; getting that wrong corrupts every app that scrolls a
    /// sub-region (which is most of them).
    fn insert_lines(&mut self, n: u16) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bot {
            return;
        }
        self.scroll_region_down(self.cy, self.scroll_bot, n);
        self.cx = 0;
        self.wrap_pending = false;
    }

    /// DL — delete `n` lines at the cursor, pulling the rest of the region up.
    fn delete_lines(&mut self, n: u16) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bot {
            return;
        }
        self.scroll_region_up(self.cy, self.scroll_bot, n);
        self.cx = 0;
        self.wrap_pending = false;
    }

    // ---- in-line editing ---------------------------------------------------

    /// ICH — insert `n` blanks at the cursor, shifting the tail of the line right.
    fn insert_chars(&mut self, n: u16) {
        let room = self.cols - self.cx;
        let n = n.min(room);
        if n == 0 {
            return;
        }
        let cols = self.cols as usize;
        let row = self.cy as usize * cols;
        let x = self.cx as usize;
        let n = n as usize;
        self.grid.copy_within(row + x..row + cols - n, row + x + n);
        let blank = self.blank();
        self.grid[row + x..row + x + n].fill(blank);
        self.heal_span(self.cy, self.cx, self.cols);
        self.heal_row_tail(self.cy);
    }

    /// DCH — delete `n` characters at the cursor, pulling the tail of the line
    /// left. Shells use this constantly for line editing; without it, deleted
    /// characters simply stay on screen.
    fn delete_chars(&mut self, n: u16) {
        let room = self.cols - self.cx;
        let n = n.min(room);
        if n == 0 {
            return;
        }
        let cols = self.cols as usize;
        let row = self.cy as usize * cols;
        let x = self.cx as usize;
        let n = n as usize;
        self.grid.copy_within(row + x + n..row + cols, row + x);
        let blank = self.blank();
        self.grid[row + cols - n..row + cols].fill(blank);
        self.heal_span(self.cy, self.cx, self.cols);
        self.heal_row_tail(self.cy);
    }

    /// ECH — blank `n` cells at the cursor without moving anything.
    fn erase_chars(&mut self, n: u16) {
        let end = self.cx.saturating_add(n).min(self.cols);
        let blank = self.blank();
        let row = self.cy as usize * self.cols as usize;
        self.grid[row + self.cx as usize..row + end as usize].fill(blank);
        self.heal_span(self.cy, self.cx, end);
    }

    // ---- printing ----------------------------------------------------------

    fn put(&mut self, c: char) {
        // Zero-width codepoints (combining marks, ZWJ, variation selectors) have
        // no cell of their own. We store one `char` per cell, so there is nowhere
        // to attach them — dropping them keeps the grid aligned, whereas giving
        // them a cell would shift the rest of the line.
        let cw = match UnicodeWidthChar::width(c) {
            Some(w) if w > 0 => w as u16,
            _ => return,
        };

        // Deferred wrap left over from the previous glyph.
        if self.wrap_pending {
            if self.autowrap {
                self.cx = 0;
                self.line_feed();
            }
            self.wrap_pending = false;
        }

        // A double-width glyph never straddles the right margin.
        if cw == 2 && self.cx + 1 >= self.cols {
            if !self.autowrap || self.cols < 2 {
                return;
            }
            let blank = self.blank();
            let i = self.idx(self.cx, self.cy);
            self.clear_wide_partner(self.cx, self.cy);
            self.grid[i] = blank;
            self.cx = 0;
            self.line_feed();
        }

        if self.insert_mode {
            self.insert_chars(cw);
        }

        self.clear_wide_partner(self.cx, self.cy);
        if cw == 2 {
            self.clear_wide_partner(self.cx + 1, self.cy);
        }

        let mut cell = self.pen;
        cell.ch = self.map_charset(c);
        cell.width = cw as u8;
        let idx = self.idx(self.cx, self.cy);
        self.grid[idx] = cell;
        if cw == 2 {
            let mut cont = self.blank();
            cont.width = 0;
            self.grid[idx + 1] = cont;
        }
        self.last_char = c;

        let next = self.cx + cw;
        if next >= self.cols {
            // Park on the last column; wrap only when the next glyph arrives.
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cx = next;
        }
    }

    fn map_charset(&self, c: char) -> char {
        match self.charsets[self.active_charset] {
            Charset::Ascii => c,
            Charset::DecSpecial => dec_special(c),
        }
    }

    fn line_feed(&mut self) {
        if self.cy == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    /// Reverse index: move up, scrolling the region if already at its top.
    fn reverse_index(&mut self) {
        if self.cy == self.scroll_top {
            self.scroll_down(1);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    // ---- erasing -----------------------------------------------------------

    fn erase_in_line(&mut self, mode: u16) {
        let (start, end) = match mode {
            0 => (self.cx, self.cols), // cursor -> EOL
            1 => (0, (self.cx + 1).min(self.cols)), // BOL -> cursor
            2 => (0, self.cols),       // whole line
            _ => return,
        };
        let blank = self.blank();
        let row = self.cy as usize * self.cols as usize;
        self.grid[row + start as usize..row + end as usize].fill(blank);
        self.heal_span(self.cy, start, end);
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        let cols = self.cols as usize;
        match mode {
            0 => {
                self.erase_in_line(0);
                let from = (self.cy as usize + 1) * cols;
                if from <= self.grid.len() {
                    self.grid[from..].fill(blank);
                }
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

    // ---- cursor addressing -------------------------------------------------

    /// Lowest / highest row the cursor may address, honouring DECOM.
    fn row_bounds(&self) -> (u16, u16) {
        if self.origin_mode {
            (self.scroll_top, self.scroll_bot)
        } else {
            (0, self.rows - 1)
        }
    }

    fn set_row(&mut self, row0: u16) {
        let (lo, hi) = self.row_bounds();
        self.cy = lo.saturating_add(row0).min(hi);
    }

    fn set_col(&mut self, col0: u16) {
        self.cx = col0.min(self.cols - 1);
        self.wrap_pending = false;
    }

    fn cursor_up(&mut self, n: u16) {
        // Stops at the top of the scroll region when starting inside it.
        let floor = if self.cy >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cy = self.cy.saturating_sub(n).max(floor);
    }

    fn cursor_down(&mut self, n: u16) {
        let ceil = if self.cy <= self.scroll_bot {
            self.scroll_bot
        } else {
            self.rows - 1
        };
        self.cy = self.cy.saturating_add(n).min(ceil);
    }

    // ---- tabs --------------------------------------------------------------

    fn tab_forward(&mut self, n: u16) {
        for _ in 0..n.max(1) {
            let mut x = self.cx + 1;
            while x < self.cols && !self.tabs.get(x as usize).copied().unwrap_or(false) {
                x += 1;
            }
            self.cx = x.min(self.cols - 1);
        }
        self.wrap_pending = false;
    }

    fn tab_back(&mut self, n: u16) {
        for _ in 0..n.max(1) {
            let mut x = self.cx;
            while x > 0 {
                x -= 1;
                if self.tabs.get(x as usize).copied().unwrap_or(false) {
                    break;
                }
            }
            self.cx = x;
        }
        self.wrap_pending = false;
    }

    // ---- saved cursor / resets ---------------------------------------------

    fn save_cursor(&mut self) {
        self.saved_cursor = SavedCursor {
            cx: self.cx,
            cy: self.cy,
            pen: self.pen,
            origin_mode: self.origin_mode,
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
    }

    fn restore_cursor(&mut self) {
        let s = self.saved_cursor;
        self.cx = s.cx.min(self.cols - 1);
        self.cy = s.cy.min(self.rows - 1);
        self.pen = s.pen;
        self.origin_mode = s.origin_mode;
        self.charsets = s.charsets;
        self.active_charset = s.active_charset;
        self.wrap_pending = false;
    }

    /// DECSTR — soft reset: attributes and modes back to defaults, contents kept.
    fn soft_reset(&mut self) {
        self.pen = Cell::default();
        self.origin_mode = false;
        self.insert_mode = false;
        self.autowrap = true;
        self.cursor_visible = true;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.charsets = [Charset::Ascii; 2];
        self.active_charset = 0;
        self.saved_cursor = SavedCursor::default();
        self.wrap_pending = false;
    }

    /// RIS — full reset. Deliberately preserves `responses`: a pending DA reply
    /// the shell is still blocking on must survive a reset that arrives in the
    /// same chunk.
    fn hard_reset(&mut self) {
        let pending = std::mem::take(&mut self.responses);
        let (cols, rows) = (self.cols, self.rows);
        *self = Screen::new(cols, rows);
        self.responses = pending;
    }

    // ---- alternate screen ---------------------------------------------------

    /// DECSET 1049 / 1047 / 47 — switch to the alternate screen. Full-screen apps
    /// (vim, less, htop, fzf) live here; without it, quitting them leaves their
    /// painted screen behind and the shell prompt draws on top of the wreckage.
    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt.is_some() {
            return;
        }
        if save_cursor {
            self.save_cursor();
        }
        let fresh = vec![self.blank(); self.grid.len()];
        self.alt = Some(Box::new(SavedScreen {
            grid: std::mem::replace(&mut self.grid, fresh),
            cx: self.cx,
            cy: self.cy,
            pen: self.pen,
            scroll_top: self.scroll_top,
            scroll_bot: self.scroll_bot,
            saved_cursor: self.saved_cursor,
        }));
        self.cx = 0;
        self.cy = 0;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.wrap_pending = false;
    }

    fn leave_alt_screen(&mut self, restore_cursor: bool) {
        let Some(saved) = self.alt.take() else {
            return;
        };
        let saved = *saved;
        self.grid = saved.grid;
        self.pen = saved.pen;
        self.scroll_top = saved.scroll_top.min(self.rows - 1);
        self.scroll_bot = saved.scroll_bot.min(self.rows - 1);
        self.saved_cursor = saved.saved_cursor;
        self.cx = saved.cx.min(self.cols - 1);
        self.cy = saved.cy.min(self.rows - 1);
        self.wrap_pending = false;
        if restore_cursor {
            self.restore_cursor();
        }
    }

    // ---- DEC private modes --------------------------------------------------

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.app_cursor_keys = on,   // DECCKM
            6 => {
                // DECOM — origin mode also homes the cursor.
                self.origin_mode = on;
                self.cx = 0;
                self.cy = if on { self.scroll_top } else { 0 };
                self.wrap_pending = false;
            }
            7 => self.autowrap = on,          // DECAWM
            25 => self.cursor_visible = on,   // DECTCEM
            47 | 1047 => {
                if on {
                    self.enter_alt_screen(false)
                } else {
                    self.leave_alt_screen(false)
                }
            }
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => {
                if on {
                    self.enter_alt_screen(true)
                } else {
                    self.leave_alt_screen(true)
                }
            }
            2004 => self.bracketed_paste = on,
            _ => {}
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        if self.responses.len() + bytes.len() <= MAX_RESPONSES {
            self.responses.extend_from_slice(bytes);
        }
    }
}

fn default_tabs(cols: u16) -> Vec<bool> {
    (0..cols as usize).map(|x| x % 8 == 0 && x != 0).collect()
}

/// Copy a grid into a differently sized one, top-anchored.
fn regrid(src: &[Cell], src_cols: u16, src_rows: u16, cols: u16, rows: u16) -> Vec<Cell> {
    let mut out = vec![Cell::default(); cols as usize * rows as usize];
    let copy_rows = rows.min(src_rows) as usize;
    let copy_cols = cols.min(src_cols) as usize;
    for y in 0..copy_rows {
        let s = y * src_cols as usize;
        let d = y * cols as usize;
        out[d..d + copy_cols].copy_from_slice(&src[s..s + copy_cols]);
    }
    out
}

/// DEC Special Graphics: the line-drawing set ncurses selects with `ESC ( 0`.
fn dec_special(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        other => other,
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

/// Raw `idx`-th parameter with no zero-elision (for ops where 0 is meaningful).
fn raw_arg(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|s| s.first().copied())
        .unwrap_or(default)
}

/// Extended colour from **colon** sub-parameters, e.g. `38:2::r:g:b` or `38:5:n`.
/// Modern apps emit this form; parsing it as if it were the semicolon form ate
/// the following parameters and produced wrong colours for the rest of the line.
fn ext_color_sub(sub: &[u16]) -> Option<Color> {
    match sub.first().copied()? {
        5 => sub.get(1).map(|&n| Color::Indexed(n as u8)),
        2 => {
            // With colour-space id: [2, id, r, g, b]. Without: [2, r, g, b].
            let (r, g, b) = if sub.len() >= 5 {
                (sub[2], sub[3], sub[4])
            } else if sub.len() >= 4 {
                (sub[1], sub[2], sub[3])
            } else {
                return None;
            };
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

/// Extended colour from **semicolon** parameters, consuming from `iter`.
fn ext_color_iter<'a>(iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    let next = |it: &mut dyn Iterator<Item = &'a [u16]>| it.next().and_then(|s| s.first().copied());
    match next(iter)? {
        5 => next(iter).map(|n| Color::Indexed(n as u8)),
        2 => {
            let r = next(iter).unwrap_or(0) as u8;
            let g = next(iter).unwrap_or(0) as u8;
            let b = next(iter).unwrap_or(0) as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

impl Screen {
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
                3 => self.pen.italic = true,
                4 => self.pen.underline = p.get(1) != Some(&0), // `4:0` = no underline
                5 | 6 => self.pen.blink = true,
                7 => self.pen.reverse = true,
                9 => self.pen.strike = true,
                21 => self.pen.underline = true, // double underline
                22 => {
                    self.pen.bold = false;
                    self.pen.dim = false;
                }
                23 => self.pen.italic = false,
                24 => self.pen.underline = false,
                25 => self.pen.blink = false,
                27 => self.pen.reverse = false,
                29 => self.pen.strike = false,
                30..=37 => self.pen.fg = ansi_color(code - 30),
                39 => self.pen.fg = Color::Reset,
                40..=47 => self.pen.bg = ansi_color(code - 40),
                49 => self.pen.bg = Color::Reset,
                90..=97 => self.pen.fg = ansi_bright(code - 90),
                100..=107 => self.pen.bg = ansi_bright(code - 100),
                38 | 48 | 58 => {
                    // Sub-params inside this param = colon form; otherwise the
                    // colour spans the following semicolon-separated params.
                    let color = if p.len() > 1 {
                        ext_color_sub(&p[1..])
                    } else {
                        ext_color_iter(&mut iter)
                    };
                    match (code, color) {
                        (38, Some(c)) => self.pen.fg = c,
                        (48, Some(c)) => self.pen.bg = c,
                        _ => {} // 58 = underline colour, not modelled
                    }
                }
                59 => {}
                _ => {}
            }
        }
    }
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
            0x09 => self.tab_forward(1),
            0x0A..=0x0C => {
                self.line_feed();
                self.wrap_pending = false;
            }
            0x0D => {
                self.cx = 0;
                self.wrap_pending = false;
            }
            0x0E => self.active_charset = 1, // SO — shift out to G1
            0x0F => self.active_charset = 0, // SI — shift in to G0
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Device queries may carry a '>' / '?' marker — handle them first.
        match action {
            'c' if intermediates.first() != Some(&b'?') => {
                if intermediates.first() == Some(&b'>') {
                    // Secondary DA (DA2): terminal-type ; version ; 0.
                    self.reply(b"\x1b[>0;95;0c");
                } else {
                    // Primary DA (DA1): advertise VT220 + ANSI colour (22).
                    self.reply(b"\x1b[?62;22c");
                }
                return;
            }
            'n' => {
                let private = intermediates.first() == Some(&b'?');
                match arg(params, 0, 0) {
                    5 if !private => self.reply(b"\x1b[0n"),
                    6 => {
                        // Cursor Position Report, 1-based; echo the '?' for DECXCPR.
                        // Under DECOM the row is reported relative to the region.
                        let (lo, _) = self.row_bounds();
                        let row = self.cy.saturating_sub(lo) + 1;
                        let prefix: &[u8] = if private { b"\x1b[?" } else { b"\x1b[" };
                        self.reply(prefix);
                        let body = format!("{};{}R", row, self.cx + 1);
                        self.reply(body.as_bytes());
                    }
                    _ => {}
                }
                return;
            }
            // Window manipulation reports. `CSI 18 t` (text area size) is another
            // query apps block on; answer it rather than let them time out.
            't' if intermediates.is_empty() => {
                match raw_arg(params, 0, 0) {
                    14 => {
                        // Pixel size — we have no cell metrics, so report a
                        // conventional 8x16 cell.
                        let body = format!("\x1b[4;{};{}t", self.rows as u32 * 16, self.cols as u32 * 8);
                        self.reply(body.as_bytes());
                    }
                    18 => {
                        let body = format!("\x1b[8;{};{}t", self.rows, self.cols);
                        self.reply(body.as_bytes());
                    }
                    _ => {}
                }
                return;
            }
            // Kitty keyboard protocol management. These all carry a private
            // marker, so a *bare* `CSI u` (DECRC) still falls through to the
            // cursor-restore arm below — keep that distinction intact.
            'u' if !intermediates.is_empty() => {
                match intermediates.first() {
                    // Query: report the current flags so the app knows we speak
                    // the protocol and enables it.
                    Some(&b'?') => {
                        self.reply(b"\x1b[?");
                        let flags = self.kitty_keyboard.to_string();
                        self.reply(flags.as_bytes());
                        self.reply(b"u");
                    }
                    // Push: save current, adopt the new flags.
                    Some(&b'>') => {
                        if self.kitty_stack.len() < MAX_KITTY_STACK {
                            self.kitty_stack.push(self.kitty_keyboard);
                        }
                        self.kitty_keyboard = arg(params, 0, 0) as u8;
                    }
                    // Pop N levels (default 1).
                    Some(&b'<') => {
                        for _ in 0..arg(params, 0, 1).min(MAX_KITTY_STACK as u16) {
                            self.kitty_keyboard = self.kitty_stack.pop().unwrap_or(0);
                        }
                    }
                    // Set with mode: 1=replace (default), 2=or, 3=and-not.
                    Some(&b'=') => {
                        let flags = arg(params, 0, 0) as u8;
                        match arg(params, 1, 1) {
                            2 => self.kitty_keyboard |= flags,
                            3 => self.kitty_keyboard &= !flags,
                            _ => self.kitty_keyboard = flags,
                        }
                    }
                    _ => {}
                }
                return;
            }
            _ => {}
        }

        // DEC private mode set/reset `ESC[?<n>h` / `ESC[?<n>l`. Several of these
        // are load-bearing (alternate screen, cursor visibility, auto-wrap,
        // application cursor keys); the rest are ignored.
        if intermediates.first() == Some(&b'?') && (action == 'h' || action == 'l') {
            let on = action == 'h';
            for p in params.iter() {
                if let Some(&mode) = p.first() {
                    self.set_private_mode(mode, on);
                }
            }
            return;
        }

        // DECSTR soft reset arrives as `CSI ! p`.
        if intermediates.first() == Some(&b'!') && action == 'p' {
            self.soft_reset();
            return;
        }

        // Any other CSI carrying an intermediate / private marker — e.g.
        // modifyOtherKeys `ESC[>4;2m`, Kitty keyboard `ESC[>1u`, DECRQM
        // `ESC[?1$p` — is NOT a standard SGR/cursor op. Ignoring it is essential:
        // otherwise `ESC[>4;2m` gets misparsed as SGR "4" (underline), which
        // sticks underline on for everything after it.
        if !intermediates.is_empty() {
            return;
        }

        match action {
            '@' => self.insert_chars(arg(params, 0, 1)),
            'A' => self.cursor_up(arg(params, 0, 1)),
            'B' | 'e' => self.cursor_down(arg(params, 0, 1)),
            'C' | 'a' => {
                self.cx = self.cx.saturating_add(arg(params, 0, 1)).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'D' => {
                self.cx = self.cx.saturating_sub(arg(params, 0, 1));
                self.wrap_pending = false;
            }
            'E' => {
                self.cursor_down(arg(params, 0, 1));
                self.cx = 0;
                self.wrap_pending = false;
            }
            'F' => {
                self.cursor_up(arg(params, 0, 1));
                self.cx = 0;
                self.wrap_pending = false;
            }
            'G' | '`' => self.set_col(arg(params, 0, 1) - 1),
            'd' => self.set_row(arg(params, 0, 1) - 1),
            'H' | 'f' => {
                self.set_row(arg(params, 0, 1) - 1);
                self.set_col(arg(params, 1, 1) - 1);
            }
            'I' => self.tab_forward(arg(params, 0, 1)),
            'Z' => self.tab_back(arg(params, 0, 1)),
            'J' => self.erase_in_display(raw_arg(params, 0, 0)),
            'K' => self.erase_in_line(raw_arg(params, 0, 0)),
            'L' => self.insert_lines(arg(params, 0, 1)),
            'M' => self.delete_lines(arg(params, 0, 1)),
            'P' => self.delete_chars(arg(params, 0, 1)),
            'S' => self.scroll_up(arg(params, 0, 1)),
            'T' => self.scroll_down(arg(params, 0, 1)),
            'X' => self.erase_chars(arg(params, 0, 1)),
            'b' => {
                // REP — repeat the last printed glyph. Bounded so a huge
                // parameter can't spin for ages.
                let n = arg(params, 0, 1).min(self.cols.saturating_mul(2));
                let c = self.last_char;
                for _ in 0..n {
                    self.put(c);
                }
            }
            'g' => match raw_arg(params, 0, 0) {
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {
                    if let Some(t) = self.tabs.get_mut(self.cx as usize) {
                        *t = false;
                    }
                }
            },
            'h' | 'l' => {
                // IRM is the only ANSI mode we model.
                if raw_arg(params, 0, 0) == 4 {
                    self.insert_mode = action == 'h';
                }
            }
            'r' => {
                // DECSTBM. A region is only valid when it has at least two rows
                // and fits on screen; anything else is ignored per spec. Without
                // this guard an inverted region underflows the scroll maths.
                let top = arg(params, 0, 1).saturating_sub(1);
                let bot = arg(params, 1, self.rows).saturating_sub(1);
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                    self.cx = 0;
                    self.cy = if self.origin_mode { top } else { 0 };
                    self.wrap_pending = false;
                }
            }
            'm' => self.apply_sgr(params),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Charset designation: `ESC ( <set>` picks G0, `ESC ) <set>` picks G1.
        match intermediates.first() {
            Some(&b'(') | Some(&b')') => {
                let slot = usize::from(intermediates[0] == b')');
                self.charsets[slot] = match byte {
                    b'0' => Charset::DecSpecial,
                    _ => Charset::Ascii,
                };
                return;
            }
            // `ESC # 8` (DECALN) and other intermediates: ignore.
            Some(_) => return,
            None => {}
        }

        match byte {
            b'D' => self.line_feed(),  // IND
            b'E' => {
                // NEL
                self.line_feed();
                self.cx = 0;
                self.wrap_pending = false;
            }
            b'H' => {
                // HTS — set a tab stop at the cursor.
                if let Some(t) = self.tabs.get_mut(self.cx as usize) {
                    *t = true;
                }
            }
            b'M' => self.reverse_index(),
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'c' => self.hard_reset(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(kind) = params.first() else { return };
        match *kind {
            b"0" | b"1" | b"2" => {
                if let Some(text) = params.get(1) {
                    let mut title = String::from_utf8_lossy(text).into_owned();
                    // Truncate on a char boundary so a long title can't grow a
                    // pane without bound.
                    if title.chars().count() > MAX_TITLE {
                        title = title.chars().take(MAX_TITLE).collect();
                    }
                    self.title = Some(title);
                }
            }
            // Foreground / background / cursor colour queries. Apps (neovim,
            // delta, bat) send these to pick a light or dark theme and wait for
            // the answer; an unanswered query is a visible startup stall, so we
            // report a conventional dark palette.
            b"10" | b"11" | b"12" if params.get(1) == Some(&&b"?"[..]) => {
                let color: &str = if *kind == b"11" {
                    "rgb:0000/0000/0000"
                } else {
                    "rgb:ffff/ffff/ffff"
                };
                let kind = String::from_utf8_lossy(kind).into_owned();
                let body = format!("\x1b]{kind};{color}\x1b\\");
                self.reply(body.as_bytes());
            }
            _ => {}
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

    fn feed(screen: &mut Screen, input: &[u8]) {
        let mut parser = Parser::new();
        for &b in input {
            parser.advance(screen, b);
        }
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

    /// Shrinking must keep the cursor (and therefore the prompt) on screen by
    /// dropping rows from the top, the way a real terminal does.
    #[test]
    fn shrinking_keeps_the_cursor_visible() {
        let mut s = render(10, 5, b"a\r\nb\r\nc\r\nd\r\ne");
        assert_eq!(s.cy, 4);
        s.resize(10, 3);
        assert_eq!(row_text(&s, 2).trim_end(), "e");
        assert_eq!(s.cy, 2, "cursor must still be on the last written row");
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

    #[test]
    fn answers_text_area_size_report() {
        let s = render(90, 30, b"\x1b[18t");
        assert_eq!(s.responses, b"\x1b[8;30;90t");
    }

    #[test]
    fn answers_background_colour_query() {
        let s = render(20, 5, b"\x1b]11;?\x07");
        assert_eq!(s.responses, b"\x1b]11;rgb:0000/0000/0000\x1b\\");
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

    #[test]
    fn tracks_bracketed_paste_mode() {
        // Off by default; DECSET 2004 enables, DECRST 2004 disables.
        let s = render(20, 5, b"");
        assert!(!s.bracketed_paste);
        let s = render(20, 5, b"\x1b[?2004h");
        assert!(s.bracketed_paste);
        let s = render(20, 5, b"\x1b[?2004h\x1b[?2004l");
        assert!(!s.bracketed_paste);
    }

    #[test]
    fn tracks_kitty_keyboard_flags() {
        // Push flags 1, then query: the reply reports the current flags.
        let s = render(20, 5, b"\x1b[>1u\x1b[?u");
        assert_eq!(s.kitty_keyboard, 1);
        assert_eq!(s.responses, b"\x1b[?1u");
        // Pop restores the previous (disabled) state.
        let s = render(20, 5, b"\x1b[>1u\x1b[<u");
        assert_eq!(s.kitty_keyboard, 0);
    }

    #[test]
    fn bare_csi_u_is_still_cursor_restore_with_kitty_support() {
        // Regression guard: DECRC (bare `ESC[u`, no marker) must keep restoring
        // the cursor even though we now special-case marked `u` sequences.
        let s = render(20, 5, b"\x1b[s\x1b[3;5H\x1b[u");
        assert_eq!((s.cx, s.cy), (0, 0));
        assert_eq!(s.kitty_keyboard, 0);
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

    // ---- newly emulated behaviour -----------------------------------------

    /// DCH is how every shell deletes a character mid-line. Without it the
    /// deleted text just stays on screen.
    #[test]
    fn delete_chars_pulls_the_line_left() {
        let s = render(20, 2, b"abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(row_text(&s, 0).trim_end(), "abef");
    }

    #[test]
    fn insert_chars_pushes_the_line_right() {
        let s = render(10, 2, b"abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(row_text(&s, 0).trim_end(), "ab  cdef");
    }

    #[test]
    fn erase_chars_blanks_in_place() {
        let s = render(10, 2, b"abcdef\x1b[1;3H\x1b[2X");
        assert_eq!(row_text(&s, 0).trim_end(), "ab  ef");
    }

    /// IL/DL operate from the **cursor row**, not from the top of the scroll
    /// region. Anchoring them at `scroll_top` corrupted every app that scrolls a
    /// sub-region.
    #[test]
    fn insert_and_delete_lines_start_at_the_cursor() {
        let s = render(10, 4, b"a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[L");
        assert_eq!(row_text(&s, 0).trim_end(), "a");
        assert_eq!(row_text(&s, 1).trim_end(), "");
        assert_eq!(row_text(&s, 2).trim_end(), "b");

        let s = render(10, 4, b"a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[M");
        assert_eq!(row_text(&s, 0).trim_end(), "a");
        assert_eq!(row_text(&s, 1).trim_end(), "c");
        assert_eq!(row_text(&s, 2).trim_end(), "d");
    }

    /// Quitting vim/less must reveal the shell exactly as it was.
    #[test]
    fn alternate_screen_round_trips() {
        let mut s = render(20, 4, b"prompt$ vim");
        feed(&mut s, b"\x1b[?1049h");
        assert_eq!(row_text(&s, 0).trim_end(), "", "alt screen starts blank");
        feed(&mut s, b"~ editor content");
        feed(&mut s, b"\x1b[?1049l");
        assert_eq!(row_text(&s, 0).trim_end(), "prompt$ vim");
        assert_eq!((s.cx, s.cy), (11, 0), "cursor restored with the screen");
    }

    /// An inverted scroll region used to underflow the region maths.
    #[test]
    fn inverted_scroll_region_is_ignored() {
        let s = render(10, 6, b"\x1b[5;2r\x1b[6n");
        assert_eq!(s.scroll_top, 0);
        assert_eq!(s.scroll_bot, 5);
    }

    #[test]
    fn huge_cursor_movement_does_not_overflow() {
        let s = render(10, 5, b"\x1b[65535B\x1b[65535C");
        assert_eq!((s.cx, s.cy), (9, 4));
    }

    /// `ESC ( 0` is how ncurses draws boxes on xterm-256color.
    #[test]
    fn dec_special_graphics_draws_lines() {
        let s = render(10, 2, b"\x1b(0qqq\x1b(Bx");
        assert_eq!(row_text(&s, 0).trim_end(), "───x");
    }

    #[test]
    fn shift_out_selects_g1() {
        let s = render(10, 2, b"\x1b)0\x0eqq\x0fa");
        assert_eq!(row_text(&s, 0).trim_end(), "──a");
    }

    /// Colon-separated truecolour (`38:2::r:g:b`) is what modern apps emit;
    /// parsing it as the semicolon form ate the following parameters.
    #[test]
    fn colon_form_truecolor_is_parsed() {
        let s = render(10, 2, b"\x1b[38:2::255:128:0mX\x1b[1mY");
        assert_eq!(s.cell(0, 0).unwrap().fg, Color::Rgb(255, 128, 0));
        assert!(s.cell(1, 0).unwrap().bold, "following SGR must still apply");

        let s = render(10, 2, b"\x1b[38:5:200mX");
        assert_eq!(s.cell(0, 0).unwrap().fg, Color::Indexed(200));
    }

    #[test]
    fn semicolon_form_truecolor_still_works() {
        let s = render(10, 2, b"\x1b[38;2;10;20;30;1mX");
        assert_eq!(s.cell(0, 0).unwrap().fg, Color::Rgb(10, 20, 30));
        assert!(s.cell(0, 0).unwrap().bold);
    }

    /// A double-width glyph claims two columns, so the cells after it stay
    /// aligned with what the terminal actually draws.
    #[test]
    fn wide_glyphs_occupy_two_cells() {
        let s = render(10, 2, "日本a".as_bytes());
        assert_eq!(s.cell(0, 0).unwrap().ch, '日');
        assert_eq!(s.cell(0, 0).unwrap().width, 2);
        assert_eq!(s.cell(1, 0).unwrap().width, 0, "continuation cell");
        assert_eq!(s.cell(2, 0).unwrap().ch, '本');
        assert_eq!(s.cell(4, 0).unwrap().ch, 'a');
        assert_eq!(s.cx, 5);
    }

    #[test]
    fn wide_glyph_wraps_rather_than_straddling_the_margin() {
        // Width 3: after "ab" only one column is left, so the wide glyph moves
        // to the next row rather than spilling past the margin.
        let s = render(3, 3, "ab日".as_bytes());
        assert_eq!(row_text(&s, 0), "ab ");
        assert_eq!(s.cell(0, 1).unwrap().ch, '日');
        assert_eq!(s.cell(1, 1).unwrap().width, 0);

        // It still fills the last two columns exactly when they are free.
        let s = render(3, 3, "a日".as_bytes());
        assert_eq!(s.cell(1, 0).unwrap().ch, '日');
        assert_eq!(s.cell(2, 0).unwrap().width, 0);
    }

    #[test]
    fn overwriting_half_a_wide_glyph_clears_its_partner() {
        let s = render(10, 2, "日\x1b[1;1Hx".as_bytes());
        assert_eq!(s.cell(0, 0).unwrap().ch, 'x');
        assert_eq!(s.cell(1, 0).unwrap().ch, ' ');
        assert_eq!(s.cell(1, 0).unwrap().width, 1);
    }

    #[test]
    fn zero_width_marks_do_not_shift_the_line() {
        // A combining acute must not consume a column of its own.
        let s = render(10, 2, "e\u{0301}x".as_bytes());
        assert_eq!(s.cell(1, 0).unwrap().ch, 'x');
        assert_eq!(s.cx, 2);
    }

    /// Narrowing the grid can cut a double-width glyph in half; the orphaned
    /// lead must be blanked or the renderer draws two columns over one.
    #[test]
    fn narrowing_splits_a_wide_glyph_cleanly() {
        let mut s = render(6, 2, "ab日cd".as_bytes());
        assert_eq!(s.cell(2, 0).unwrap().width, 2);
        s.resize(3, 2);
        assert_eq!(s.cell(2, 0).unwrap().ch, ' ');
        assert_eq!(s.cell(2, 0).unwrap().width, 1);
    }

    /// SU/SD move the scroll region, unlike IL/DL which start at the cursor.
    #[test]
    fn scroll_up_and_down_act_on_the_region() {
        let s = render(10, 4, b"a\r\nb\r\nc\r\nd\x1b[1;3r\x1b[S");
        assert_eq!(row_text(&s, 0).trim_end(), "b");
        assert_eq!(row_text(&s, 1).trim_end(), "c");
        assert_eq!(row_text(&s, 2).trim_end(), "");
        assert_eq!(row_text(&s, 3).trim_end(), "d", "outside the region");
    }

    #[test]
    fn tracks_cursor_visibility_and_app_cursor_keys() {
        let s = render(10, 2, b"\x1b[?25l\x1b[?1h");
        assert!(!s.cursor_visible);
        assert!(s.app_cursor_keys);
        let s = render(10, 2, b"\x1b[?25l\x1b[?25h");
        assert!(s.cursor_visible);
    }

    #[test]
    fn autowrap_off_overwrites_the_last_column() {
        let s = render(3, 2, b"\x1b[?7labcd");
        assert_eq!(row_text(&s, 0), "abd");
        assert_eq!(row_text(&s, 1).trim_end(), "");
    }

    #[test]
    fn combined_private_modes_in_one_sequence() {
        // Apps commonly batch modes: `ESC[?1;7;25h`.
        let s = render(10, 2, b"\x1b[?1;7;25h");
        assert!(s.app_cursor_keys && s.autowrap && s.cursor_visible);
    }

    #[test]
    fn soft_reset_restores_defaults() {
        let s = render(10, 4, b"\x1b[31m\x1b[?7l\x1b[2;3r\x1b[!p");
        assert_eq!(s.pen.fg, Color::Reset);
        assert!(s.autowrap);
        assert_eq!((s.scroll_top, s.scroll_bot), (0, 3));
    }

    #[test]
    fn repeat_last_glyph() {
        let s = render(10, 2, b"-\x1b[4b");
        assert_eq!(row_text(&s, 0).trim_end(), "-----");
    }

    #[test]
    fn tab_stops_can_be_set_and_cleared() {
        // Clear all stops, set one at column 5, then tab to it.
        let s = render(20, 2, b"\x1b[3g\x1b[1;6H\x1bH\x1b[1;1H\tX");
        assert_eq!(s.cell(5, 0).unwrap().ch, 'X');
    }

    #[test]
    fn hard_reset_preserves_pending_replies() {
        // A DA query answered in the same chunk as a reset must not be lost —
        // the shell is blocking on that reply.
        let s = render(10, 2, b"\x1b[c\x1bc");
        assert_eq!(s.responses, b"\x1b[?62;22c");
    }

    #[test]
    fn resize_while_on_the_alternate_screen_keeps_the_primary() {
        let mut s = render(20, 4, b"shell prompt");
        feed(&mut s, b"\x1b[?1049h");
        s.resize(30, 6);
        feed(&mut s, b"\x1b[?1049l");
        assert_eq!(row_text(&s, 0).trim_end(), "shell prompt");
        assert_eq!((s.cols, s.rows), (30, 6));
    }

    /// Fuzz-ish guard: no byte sequence may panic the emulator. A panic in the
    /// server takes every shell in the session down with it.
    #[test]
    fn arbitrary_byte_soup_never_panics() {
        let mut screen = Screen::new(17, 7);
        let mut parser = Parser::new();
        // A cheap deterministic PRNG — no dev-dependency needed.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..400_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            parser.advance(&mut screen, (state >> 33) as u8);
            screen.responses.clear();
        }
        // Interleave resizes, which reshape every buffer including the alt screen.
        for (c, r) in [(1, 1), (200, 60), (3, 2), (80, 24)] {
            screen.resize(c, r);
            for _ in 0..20_000 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                parser.advance(&mut screen, (state >> 33) as u8);
                screen.responses.clear();
            }
        }
    }

    /// Every escape sequence we claim to emulate, fed at a 1x1 screen where all
    /// the clamping edge cases live.
    #[test]
    fn degenerate_one_by_one_screen_never_panics() {
        let seqs: &[&[u8]] = &[
            b"\x1b[1;1r", b"\x1b[10P", b"\x1b[10@", b"\x1b[10X", b"\x1b[10L",
            b"\x1b[10M", b"\x1b[10S", b"\x1b[10T", b"\x1b[99;99H", b"\x1b[J",
            b"\x1b[1J", b"\x1b[2J", b"\x1b[K", b"\x1b[1K", b"\x1b[2K", b"\x1b[Z",
            b"\x1b[I", b"\x1b[3g", b"\x1bH", b"\x1b[?1049h", b"\x1b[?1049l",
            b"\x1b[999b", b"\x1b[?6h", b"\x1b[?7l", b"\x1b[4h", b"\x1b[!p",
            b"\x1bM", b"\x1bD", b"\x1bE", b"\x1b7", b"\x1b8", b"\x1bc",
        ];
        for size in [(1u16, 1u16), (2, 1), (1, 2), (4, 3)] {
            let mut screen = Screen::new(size.0, size.1);
            let mut parser = Parser::new();
            for seq in seqs {
                for &b in *seq {
                    parser.advance(&mut screen, b);
                }
                for &b in "日x\r\n".as_bytes() {
                    parser.advance(&mut screen, b);
                }
            }
        }
    }
}
