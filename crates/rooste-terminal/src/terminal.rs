//! The terminal state machine and a resolved, render-ready screen.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Color, NamedColor, Processor};

use crate::input::InputMode;
use crate::palette::{Palette, Rgb};

const DEFAULT_SCROLLBACK: usize = 10_000;
const DCS_TMUX_ENTER: &[u8] = b"\x1bP1000p";

/// Something the terminal wants the application to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// Bytes to send to the remote side, e.g. replies to status queries.
    Reply(Vec<u8>),
    Title(String),
    ResetTitle,
    Bell,
    /// OSC 52: the remote application put text on the clipboard.
    Clipboard(String),
    /// `tmux -CC` emitted `\033P1000p` to enter control mode; carries any trailing bytes.
    TmuxControlModeEntered(Vec<u8>),
}

/// Which mouse events the remote application asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseReport {
    #[default]
    None,
    /// Button presses and releases.
    Click,
    /// Also motion while a button is held.
    Drag,
    /// All motion.
    Motion,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseMode {
    pub report: MouseReport,
    /// SGR (1006) encoding instead of the legacy X10 bytes.
    pub sgr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    /// Character-wise.
    Simple,
    /// Whole words (double click).
    Word,
    /// Whole lines (triple click).
    Line,
    /// Rectangular.
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

/// One cell, with colors and attributes already resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// First half of a double-width character.
    pub wide: bool,
    /// Second half of a double-width character; draws nothing.
    pub spacer: bool,
}

/// The visible screen, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    /// Row-major, `rows * cols` cells.
    pub cells: Vec<ScreenCell>,
    /// Cursor position as (column, row), if visible.
    pub cursor: Option<(usize, usize)>,
    pub cursor_shape: CursorShape,
    pub background: Rgb,
    pub cursor_color: Rgb,
}

impl Screen {
    pub fn row(&self, row: usize) -> &[ScreenCell] {
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }
}

#[derive(Clone)]
struct Listener {
    events: Arc<Mutex<Vec<TermEvent>>>,
    palette: Arc<Mutex<Palette>>,
    window: Arc<Mutex<WindowSize>>,
}

impl Listener {
    fn push(&self, event: TermEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.push(TermEvent::Reply(text.into_bytes())),
            Event::Title(title) => self.push(TermEvent::Title(title)),
            Event::ResetTitle => self.push(TermEvent::ResetTitle),
            Event::Bell => self.push(TermEvent::Bell),
            Event::ClipboardStore(_, text) => self.push(TermEvent::Clipboard(text)),
            Event::ColorRequest(index, format) => {
                let palette = self.palette.lock().unwrap_or_else(|e| e.into_inner());
                let rgb = match index {
                    0..=255 => palette.indexed(index as u8),
                    257 => palette.background,
                    258 => palette.cursor,
                    _ => palette.foreground,
                };
                drop(palette);
                let reply = format(ansi::Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                });
                self.push(TermEvent::Reply(reply.into_bytes()));
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *self.window.lock().unwrap_or_else(|e| e.into_inner());
                self.push(TermEvent::Reply(format(size).into_bytes()));
            }
            // Reading the clipboard from the remote side (OSC 52 query) is
            // refused: it would leak local clipboard contents.
            _ => {}
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Which way to look for the next match: up into older output, or down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Up,
    Down,
}

/// A terminal emulator: feed it output, render its [`Screen`].
pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    /// The last search match, where the next search continues from.
    found: Option<Match>,
    /// Number of bytes matched in `\x1bP1000p` so far.
    dcs_matched: usize,
    /// True while this terminal is acting as a `tmux -CC` gateway.
    in_tmux_control_mode: bool,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("cols", &self.term.columns())
            .field("rows", &self.term.screen_lines())
            .field("in_tmux_control_mode", &self.in_tmux_control_mode)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_palette(cols, rows, Palette::default())
    }

    pub fn with_palette(cols: u16, rows: u16, palette: Palette) -> Self {
        Self::with_options(cols, rows, palette, DEFAULT_SCROLLBACK)
    }

    /// A terminal with a palette and `scrollback` lines of history.
    pub fn with_options(cols: u16, rows: u16, palette: Palette, scrollback: usize) -> Self {
        let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
        let listener = Listener {
            events: Arc::default(),
            palette: Arc::new(Mutex::new(palette)),
            window: Arc::new(Mutex::new(window_size(cols, rows, 0, 0))),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &Size { cols, rows }, listener.clone()),
            parser: Processor::new(),
            listener,
            found: None,
            dcs_matched: 0,
            in_tmux_control_mode: false,
        }
    }

    /// Whether this terminal has entered `tmux -CC` control mode.
    pub fn is_tmux_control_mode(&self) -> bool {
        self.in_tmux_control_mode
    }

    /// Leave `tmux -CC` control mode and resume normal VT emulation.
    pub fn exit_tmux_control_mode(&mut self) {
        self.in_tmux_control_mode = false;
        self.dcs_matched = 0;
    }

    /// Process output from the remote side.
    pub fn advance(&mut self, bytes: &[u8]) {
        if !self.in_tmux_control_mode {
            for (idx, &b) in bytes.iter().enumerate() {
                if b == DCS_TMUX_ENTER[self.dcs_matched] {
                    self.dcs_matched += 1;
                    if self.dcs_matched == DCS_TMUX_ENTER.len() {
                        self.dcs_matched = 0;
                        self.in_tmux_control_mode = true;
                        let prefix_end = (idx + 1).saturating_sub(DCS_TMUX_ENTER.len());
                        if prefix_end > 0 {
                            self.parser.advance(&mut self.term, &bytes[..prefix_end]);
                            self.repair_cursor_cell();
                        }
                        self.listener
                            .push(TermEvent::TmuxControlModeEntered(bytes[idx + 1..].to_vec()));
                        return;
                    }
                } else if b == DCS_TMUX_ENTER[0] {
                    self.dcs_matched = 1;
                } else {
                    self.dcs_matched = 0;
                }
            }
        }
        self.parser.advance(&mut self.term, bytes);
        self.repair_cursor_cell();
    }

    /// Some escape sequences can leave the cursor on a wide character spacer
    /// in the first column, which belongs to no character. alacritty steps the
    /// cursor back one column from a spacer, which would underflow there, so
    /// turn that cell into a normal one.
    fn repair_cursor_cell(&mut self) {
        let point = self.term.grid().cursor.point;
        if point.column.0 == 0 {
            self.term.grid_mut()[point]
                .flags
                .remove(Flags::WIDE_CHAR_SPACER);
        }
    }

    /// Events produced since the last call.
    pub fn take_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(
            &mut *self
                .listener
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    pub fn size(&self) -> (u16, u16) {
        let clamp = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
        (clamp(self.term.columns()), clamp(self.term.screen_lines()))
    }

    /// Resize the grid. `cell_width`/`cell_height` (in pixels) are only used
    /// to answer pixel-size queries.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
        *self
            .listener
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = window_size(cols, rows, cell_width, cell_height);
        if (cols, rows) != (self.term.columns(), self.term.screen_lines()) {
            self.term.resize(Size { cols, rows });
            self.repair_cursor_cell();
        }
    }

    pub fn palette(&self) -> Palette {
        self.listener
            .palette
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Switch colors, e.g. when the theme changes.
    pub fn set_palette(&mut self, palette: Palette) {
        *self
            .listener
            .palette
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = palette;
    }

    /// Change how many lines of history are kept.
    pub fn set_scrollback(&mut self, lines: usize) {
        let config = Config {
            scrolling_history: lines,
            ..Config::default()
        };
        self.term.set_options(config);
    }

    /// Key encoding modes set by the remote application.
    pub fn input_mode(&self) -> InputMode {
        InputMode {
            app_cursor: self.term.mode().contains(TermMode::APP_CURSOR),
        }
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Whether the alternate screen (full-screen apps like vim) is active.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether mouse wheel on the alternate screen should send arrow keys.
    pub fn alternate_scroll(&self) -> bool {
        self.term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
    }

    pub fn mouse_mode(&self) -> MouseMode {
        let mode = self.term.mode();
        let report = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseReport::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseReport::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseReport::Click
        } else {
            MouseReport::None
        };
        MouseMode {
            report,
            sgr: mode.contains(TermMode::SGR_MOUSE),
        }
    }

    /// Scroll the view into the scrollback by `lines` (positive = up).
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// How many lines the view is scrolled back.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines of scrollback history available.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    fn point(&self, col: usize, row: usize) -> Point {
        let offset = self.display_offset() as i32;
        let col = col.min(self.term.columns().saturating_sub(1));
        let row = row.min(self.term.screen_lines().saturating_sub(1));
        Point::new(Line(row as i32 - offset), Column(col))
    }

    /// Start a selection at a visible cell. `right_half` is true when the
    /// pointer is on the right half of the cell.
    pub fn select_start(&mut self, col: usize, row: usize, right_half: bool, kind: SelectionKind) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        };
        let side = if right_half { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(ty, self.point(col, row), side));
    }

    /// Extend the current selection to a visible cell.
    pub fn select_update(&mut self, col: usize, row: usize, right_half: bool) {
        let point = self.point(col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Find `text` in the screen and scrollback (ignoring case unless it
    /// has capitals), continuing from the last match and wrapping around.
    /// The match is selected and scrolled into view. Returns false if
    /// there is none.
    pub fn find(&mut self, text: &str, direction: SearchDirection) -> bool {
        let Ok(mut regex) = RegexSearch::new(&escape_regex(text)) else {
            return false;
        };
        let (dir, origin) = match (direction, &self.found) {
            (SearchDirection::Up, Some(found)) => (
                Direction::Left,
                found.start().sub(&self.term, Boundary::None, 1),
            ),
            (SearchDirection::Down, Some(found)) => (
                Direction::Right,
                found.end().add(&self.term, Boundary::None, 1),
            ),
            // Start below the last line, so the newest match comes first.
            (SearchDirection::Up, None) => (
                Direction::Left,
                Point::new(
                    Line(self.term.screen_lines() as i32 - 1),
                    Column(self.term.columns() - 1),
                ),
            ),
            (SearchDirection::Down, None) => (
                Direction::Right,
                Point::new(Line(-(self.history_size() as i32)), Column(0)),
            ),
        };
        let side = match dir {
            Direction::Left => Side::Right,
            Direction::Right => Side::Left,
        };
        let Some(found) = self.term.search_next(&mut regex, origin, dir, side, None) else {
            self.clear_search();
            return false;
        };
        let mut selection = Selection::new(SelectionType::Simple, *found.start(), Side::Left);
        selection.update(*found.end(), Side::Right);
        self.term.selection = Some(selection);
        self.term.scroll_to_point(*found.start());
        self.found = Some(found);
        true
    }

    /// Forget the search position and its selection. The next
    /// [`Self::find`] starts from the bottom (or top) again.
    pub fn clear_search(&mut self) {
        if self.found.take().is_some() {
            self.term.selection = None;
        }
    }

    /// Selected text, or `None` if nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// The visible screen with colors resolved against the palette.
    pub fn screen(&self) -> Screen {
        let palette = &self.palette();
        let content = self.term.renderable_content();
        let overrides = content.colors;
        let resolve = |color: Color, bold: bool| -> Rgb {
            match color {
                Color::Spec(c) => Rgb::new(c.r, c.g, c.b),
                Color::Indexed(i) => {
                    // Classic behavior: bold uses the bright ANSI variant.
                    let i = if bold && i < 8 { i + 8 } else { i };
                    overrides[usize::from(i)]
                        .map(|c| Rgb::new(c.r, c.g, c.b))
                        .unwrap_or_else(|| palette.indexed(i))
                }
                Color::Named(named) => {
                    let index = named as usize;
                    let index = if bold && index < 8 { index + 8 } else { index };
                    if let Some(c) = overrides[index] {
                        return Rgb::new(c.r, c.g, c.b);
                    }
                    match named {
                        NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
                        NamedColor::Background => palette.background,
                        NamedColor::Cursor => palette.cursor,
                        NamedColor::DimForeground => palette.foreground.dimmed(),
                        _ if index < 16 => palette.ansi[index],
                        // Dim ANSI colors.
                        _ => palette.ansi[index.saturating_sub(NamedColor::DimBlack as usize) % 8]
                            .dimmed(),
                    }
                }
            }
        };

        let (cols, rows) = (self.term.columns(), self.term.screen_lines());
        let blank = ScreenCell {
            ch: ' ',
            fg: palette.foreground,
            bg: palette.background,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide: false,
            spacer: false,
        };
        let mut cells = vec![blank; cols * rows];
        let offset = content.display_offset as i32;
        let selection = content.selection;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let (row, col) = (row as usize, indexed.point.column.0);
            let cell = &indexed.cell;
            let flags = cell.flags;
            let bold = flags.contains(Flags::BOLD);
            let mut fg = resolve(cell.fg, bold);
            let mut bg = resolve(cell.bg, false);
            if flags.contains(Flags::DIM) {
                fg = fg.dimmed();
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                fg = palette.selection_foreground;
                bg = palette.selection_background;
            }
            cells[row * cols + col] = ScreenCell {
                // A tab is kept in its cell for copying, but drawn as blank.
                ch: if cell.c == '\t' { ' ' } else { cell.c },
                fg,
                bg,
                bold,
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikeout: flags.contains(Flags::STRIKEOUT),
                wide: flags.contains(Flags::WIDE_CHAR),
                spacer: flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
            };
        }

        let cursor_point = content.cursor.point;
        let cursor_row = cursor_point.line.0 + offset;
        let visible = content.mode.contains(TermMode::SHOW_CURSOR)
            && content.cursor.shape != ansi::CursorShape::Hidden
            && (0..rows as i32).contains(&cursor_row);
        let cursor_shape = match self.term.cursor_style().shape {
            ansi::CursorShape::Underline => CursorShape::Underline,
            ansi::CursorShape::Beam => CursorShape::Beam,
            _ => CursorShape::Block,
        };

        Screen {
            cols,
            rows,
            cells,
            cursor: visible.then_some((cursor_point.column.0, cursor_row as usize)),
            cursor_shape,
            background: palette.background,
            cursor_color: palette.cursor,
        }
    }

    /// Visible text, one line per row with trailing spaces trimmed.
    pub fn screen_text(&self) -> String {
        let screen = self.screen();
        let mut out = String::new();
        for row in 0..screen.rows {
            let line: String = screen
                .row(row)
                .iter()
                .filter(|c| !c.spacer)
                .map(|c| c.ch)
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

fn window_size(cols: usize, rows: usize, cell_width: u16, cell_height: u16) -> WindowSize {
    let clamp = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
    WindowSize {
        num_cols: clamp(cols),
        num_lines: clamp(rows),
        cell_width,
        cell_height,
    }
}

/// `text` as a regex matching it literally.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
