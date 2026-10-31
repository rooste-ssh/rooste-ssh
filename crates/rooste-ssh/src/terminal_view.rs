//! The terminal widget: draws a [`Terminal`] and turns keyboard and mouse
//! input into bytes for the remote side.

use std::time::{Duration, Instant};

use egui::{
    Color32, CornerRadius, Event, FontId, Id, PointerButton, Pos2, Rect, Response, Sense, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};
use rooste_terminal::input::{self, MouseAction, MouseButton};
use rooste_terminal::{CursorShape, MouseReport, Rgb, Screen, SelectionKind, Terminal};

use crate::keys::{Action, KeyTranslator};

const MULTI_CLICK: Duration = Duration::from_millis(400);

/// What the widget wants the session to do after a frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ViewOutput {
    /// Bytes for the remote side, in order.
    pub send: Vec<u8>,
    /// The grid size changed to (columns, rows).
    pub resize: Option<(u16, u16)>,
}

/// Gap between the terminal text and the edges of its area.
const PADDING: f32 = 6.0;
/// Key in egui's data: some terminal is waiting to take the keyboard focus.
const FOCUS_PENDING: &str = "rooste-terminal-focus-pending";

/// Tell the terminals drawn next whether one of them is waiting to take the
/// keyboard focus, so the others leave it alone.
pub fn set_focus_pending(ctx: &egui::Context, pending: bool) {
    ctx.data_mut(|d| d.insert_temp(Id::unique(FOCUS_PENDING), pending));
}
const MIN_FONT_SIZE: f32 = 6.0;
const MAX_FONT_SIZE: f32 = 72.0;

/// Per-terminal widget state.
#[derive(Debug)]
pub struct TerminalView {
    pub font_size: f32,
    /// This tab's zoom, multiplying `font_size` (1.0 = 100%).
    pub zoom: f32,
    /// Copy the selection to the clipboard when the mouse button is released.
    pub copy_on_select: bool,
    keys: KeyTranslator,
    /// Fractional wheel lines not yet scrolled.
    scroll_accum: f32,
    clicks: u8,
    last_click: Option<Instant>,
    /// Mouse button currently held while reporting to the application.
    reported_button: Option<MouseButton>,
    selecting: bool,
    /// Take keyboard focus as soon as possible, e.g. when its tab is clicked.
    pub focus_requested: bool,
    /// Whether the terminal had keyboard focus when last shown.
    pub has_focus: bool,
}

impl Default for TerminalView {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            zoom: 1.0,
            copy_on_select: true,
            keys: KeyTranslator::default(),
            scroll_accum: 0.0,
            clicks: 0,
            last_click: None,
            reported_button: None,
            selecting: false,
            focus_requested: false,
            has_focus: false,
        }
    }
}

/// Cell geometry in points.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    cell: Vec2,
    origin: Pos2,
}

impl Metrics {
    /// The visible cell under `pos`, and whether `pos` is on its right half.
    fn cell_at(&self, pos: Pos2, cols: usize, rows: usize) -> (usize, usize, bool) {
        let rel = pos - self.origin;
        let x = (rel.x / self.cell.x).max(0.0);
        let y = (rel.y / self.cell.y).max(0.0);
        let col = (x as usize).min(cols.saturating_sub(1));
        let row = (y as usize).min(rows.saturating_sub(1));
        (col, row, x.fract() >= 0.5)
    }
}

fn color(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

impl TerminalView {
    fn font(&self) -> FontId {
        FontId::monospace((self.font_size * self.zoom).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE))
    }

    /// Make the text 10% larger.
    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom * 1.1).min(MAX_FONT_SIZE / self.font_size);
    }

    /// Make the text 10% smaller.
    pub fn zoom_out(&mut self) {
        self.zoom = (self.zoom / 1.1).max(MIN_FONT_SIZE / self.font_size);
    }

    pub fn reset_zoom(&mut self) {
        self.zoom = 1.0;
    }

    /// Lay out, handle input and paint. Call once per frame.
    pub fn show(&mut self, ui: &mut Ui, term: &mut Terminal) -> ViewOutput {
        let mut out = ViewOutput::default();
        let font = self.font();
        let cell = ui
            .ctx()
            .fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let available = ui.available_size().max(cell + vec2(2.0, 2.0) * PADDING);
        // The text keeps a small gap from the edges; the background fills them.
        let grid = available - vec2(2.0, 2.0) * PADDING;

        let cols = ((grid.x / cell.x).floor() as u16).max(1);
        let rows = ((grid.y / cell.y).floor() as u16).max(1);
        if term.size() != (cols, rows) {
            let ppp = ui.ctx().pixels_per_point();
            term.resize(cols, rows, (cell.x * ppp) as u16, (cell.y * ppp) as u16);
            out.resize = Some((cols, rows));
        }

        // A fixed id, so the keyboard focus stays when widgets above the
        // terminal (find bar, status lines) come and go.
        let (rect, _) = ui.allocate_exact_size(available, Sense::hover());
        let response = ui.interact(
            rect,
            ui.make_persistent_id("terminal"),
            Sense::click_and_drag(),
        );
        let response = response.on_hover_cursor(egui::CursorIcon::Text);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Terminal, true, "Terminal"));
        let id = response.id;
        // Take keyboard focus when clicked, and whenever nothing else has it
        // (e.g. after a dialog closes) unless another terminal is about to
        // take it (e.g. a tab just switched to); otherwise the first terminal
        // drawn would grab it for a moment.
        // A click elsewhere may be giving another widget focus this frame
        // (egui applies it at the end of the frame), and while a button is
        // held the focus is about to go somewhere, so don't take it then.
        let pending = ui
            .data(|d| d.get_temp::<bool>(Id::unique(FOCUS_PENDING)))
            .unwrap_or(false);
        let nothing_focused = ui.memory(|m| m.focused().is_none())
            && !pending
            && !ui.input(|i| {
                i.pointer.any_pressed() || i.pointer.any_released() || i.pointer.any_down()
            });
        if nothing_focused || self.focus_requested || response.clicked() || response.drag_started()
        {
            ui.memory_mut(|m| m.request_focus(id));
        }
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        let metrics = Metrics {
            cell,
            origin: rect.min + vec2(PADDING, PADDING),
        };

        if response.has_focus() {
            self.handle_keyboard(ui, term, &mut out);
        }
        self.handle_mouse(ui, term, &response, metrics, &mut out);

        if self.has_focus != response.has_focus() {
            // The tab bar, drawn before the terminal, shows which tab has
            // the focus; draw again so it doesn't lag a frame behind.
            ui.ctx().request_repaint();
        }
        self.has_focus = response.has_focus();
        // Ask until it has the focus: a click that opened this tab (e.g. on a
        // menu) can take the focus away again in the next frame.
        if self.has_focus {
            self.focus_requested = false;
        }
        self.paint(ui, rect, metrics, &term.screen(), self.has_focus);
        out
    }

    fn handle_keyboard(&mut self, ui: &Ui, term: &mut Terminal, out: &mut ViewOutput) {
        let (events, modifiers) = ui.input(|i| (i.events.clone(), i.modifiers));
        for event in &events {
            let has_selection = term.selection_text().is_some();
            match self
                .keys
                .translate(event, modifiers, term.input_mode(), has_selection)
            {
                Some(Action::Send(bytes)) => {
                    term.clear_selection();
                    term.scroll_to_bottom();
                    out.send.extend_from_slice(&bytes);
                }
                Some(Action::Copy) => {
                    if let Some(text) = term.selection_text() {
                        ui.ctx().copy_text(text);
                    }
                    term.clear_selection();
                }
                Some(Action::Paste(text)) => {
                    term.scroll_to_bottom();
                    out.send
                        .extend(input::encode_paste(&text, term.bracketed_paste()));
                }
                None => {}
            }
        }
    }

    fn handle_mouse(
        &mut self,
        ui: &Ui,
        term: &mut Terminal,
        response: &Response,
        metrics: Metrics,
        out: &mut ViewOutput,
    ) {
        let (cols, rows) = term.size();
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        let (events, modifiers) = ui.input(|i| (i.events.clone(), i.modifiers));
        let mods = input::Modifiers {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.ctrl,
        };
        let mode = term.mouse_mode();
        // Shift overrides application mouse reporting so text can always be
        // selected, as in xterm.
        let reporting = mode.report != MouseReport::None && !modifiers.shift;

        for event in &events {
            match event {
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if response.rect.contains(*pos) || self.selecting || !pressed => {
                    let (col, row, right) = metrics.cell_at(*pos, cols, rows);
                    let button = match button {
                        PointerButton::Primary => MouseButton::Left,
                        PointerButton::Middle => MouseButton::Middle,
                        PointerButton::Secondary => MouseButton::Right,
                        _ => continue,
                    };
                    if reporting && response.rect.contains(*pos) {
                        let action = if *pressed {
                            self.reported_button = Some(button);
                            MouseAction::Press
                        } else {
                            self.reported_button = None;
                            MouseAction::Release
                        };
                        out.send.extend(
                            input::encode_mouse(Some(button), action, col, row, mods, mode)
                                .unwrap_or_default(),
                        );
                        continue;
                    }
                    match (button, pressed) {
                        (MouseButton::Left, true) => {
                            let now = Instant::now();
                            let repeat = self
                                .last_click
                                .is_some_and(|t| now.duration_since(t) < MULTI_CLICK);
                            self.clicks = if repeat { self.clicks % 3 + 1 } else { 1 };
                            self.last_click = Some(now);
                            let kind = match self.clicks {
                                2 => SelectionKind::Word,
                                3 => SelectionKind::Line,
                                _ if modifiers.alt => SelectionKind::Block,
                                _ => SelectionKind::Simple,
                            };
                            term.select_start(col, row, right, kind);
                            self.selecting = true;
                        }
                        (MouseButton::Left, false) => {
                            self.selecting = false;
                            // Copy on select, as PuTTY does.
                            match term.selection_text() {
                                Some(text) if self.copy_on_select => ui.ctx().copy_text(text),
                                Some(_) => {}
                                None => term.clear_selection(),
                            }
                        }
                        // Right or middle click pastes, as PuTTY does.
                        (MouseButton::Right | MouseButton::Middle, true) => {
                            if let Some(text) = read_clipboard() {
                                term.scroll_to_bottom();
                                out.send
                                    .extend(input::encode_paste(&text, term.bracketed_paste()));
                            }
                        }
                        _ => {}
                    }
                }
                Event::PointerMoved(pos) => {
                    let (col, row, right) = metrics.cell_at(*pos, cols, rows);
                    if self.selecting {
                        term.select_update(col, row, right);
                    } else if reporting
                        && response.rect.contains(*pos)
                        && let Some(bytes) = input::encode_mouse(
                            self.reported_button,
                            MouseAction::Motion,
                            col,
                            row,
                            mods,
                            mode,
                        )
                    {
                        out.send.extend(bytes);
                    }
                }
                // Ctrl + wheel zooms instead of scrolling.
                Event::MouseWheel {
                    delta, modifiers, ..
                } if response.hovered() && modifiers.command => {
                    if delta.y > 0.0 {
                        self.zoom_in();
                    } else if delta.y < 0.0 {
                        self.zoom_out();
                    }
                }
                Event::MouseWheel { unit, delta, .. } if response.hovered() => {
                    let lines = match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Page => delta.y * rows as f32,
                        egui::MouseWheelUnit::Point => delta.y / metrics.cell.y,
                    };
                    self.scroll_accum += lines;
                    let whole = self.scroll_accum.trunc();
                    self.scroll_accum -= whole;
                    let whole = whole as i32;
                    if whole == 0 {
                        continue;
                    }
                    let pos = response.hover_pos().unwrap_or(metrics.origin);
                    let (col, row, _) = metrics.cell_at(pos, cols, rows);
                    let wheel = if whole > 0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    for _ in 0..whole.unsigned_abs() {
                        if reporting {
                            out.send.extend(
                                input::encode_mouse(
                                    Some(wheel),
                                    MouseAction::Press,
                                    col,
                                    row,
                                    mods,
                                    mode,
                                )
                                .unwrap_or_default(),
                            );
                        } else if term.alternate_scroll() {
                            let key = if whole > 0 {
                                input::Key::Up
                            } else {
                                input::Key::Down
                            };
                            out.send.extend(input::encode(
                                key,
                                input::Modifiers::NONE,
                                term.input_mode(),
                            ));
                        }
                    }
                    if !reporting && !term.alt_screen() {
                        term.scroll(whole);
                    }
                }
                _ => {}
            }
        }
    }

    fn paint(&self, ui: &Ui, rect: Rect, metrics: Metrics, screen: &Screen, focused: bool) {
        let painter = ui.painter_at(rect);
        let font = self.font();
        let cell = metrics.cell;
        painter.rect_filled(rect, CornerRadius::ZERO, color(screen.background));

        for row in 0..screen.rows {
            let cells = screen.row(row);
            let y = metrics.origin.y + row as f32 * cell.y;

            // Backgrounds, merged into runs of equal color.
            let mut col = 0;
            while col < cells.len() {
                let bg = cells[col].bg;
                let start = col;
                while col < cells.len() && cells[col].bg == bg {
                    col += 1;
                }
                if bg != screen.background {
                    let min = pos2(metrics.origin.x + start as f32 * cell.x, y);
                    let size = vec2((col - start) as f32 * cell.x, cell.y);
                    painter.rect_filled(Rect::from_min_size(min, size), 0.0, color(bg));
                }
            }

            // Text: ASCII runs with the same style share one galley; other
            // characters are placed cell by cell so wide glyphs line up.
            let mut col = 0;
            while col < cells.len() {
                let c = cells[col];
                if c.spacer || c.ch == ' ' {
                    col += 1;
                    continue;
                }
                let x = metrics.origin.x + col as f32 * cell.x;
                let cell_rect = Rect::from_min_size(pos2(x, y), cell);
                if crate::box_drawing::draw(&painter, cell_rect, c.ch, color(c.fg)) {
                    col += 1;
                    continue;
                }
                let mut text = String::from(c.ch);
                let mut end = col + 1;
                if c.ch.is_ascii() {
                    while end < cells.len() {
                        let n = cells[end];
                        let same = n.ch.is_ascii()
                            && n.fg == c.fg
                            && n.italic == c.italic
                            && n.underline == c.underline
                            && n.strikeout == c.strikeout;
                        if !same {
                            break;
                        }
                        text.push(n.ch);
                        end += 1;
                    }
                }
                let fg = color(c.fg);
                let mut format = egui::TextFormat::simple(font.clone(), fg);
                format.italics = c.italic;
                let mut job = egui::text::LayoutJob::single_section(text, format);
                job.wrap.max_width = f32::INFINITY;
                let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                painter.galley(pos2(x, y), galley, fg);

                let width = (end - col + usize::from(c.wide)) as f32 * cell.x;
                if c.underline {
                    let uy = y + cell.y - 1.5;
                    painter.line_segment([pos2(x, uy), pos2(x + width, uy)], Stroke::new(1.0, fg));
                }
                if c.strikeout {
                    let sy = y + cell.y * 0.55;
                    painter.line_segment([pos2(x, sy), pos2(x + width, sy)], Stroke::new(1.0, fg));
                }
                col = end;
            }
        }

        if let Some((col, row)) = screen.cursor
            && let Some(&c) = screen.row(row).get(col)
        {
            let width = if c.wide { 2.0 } else { 1.0 } * cell.x;
            let min = pos2(
                metrics.origin.x + col as f32 * cell.x,
                metrics.origin.y + row as f32 * cell.y,
            );
            let cursor_rect = Rect::from_min_size(min, vec2(width, cell.y));
            let cursor_color = color(screen.cursor_color);
            match (screen.cursor_shape, focused) {
                (_, false) => {
                    painter.rect_stroke(
                        cursor_rect.shrink(0.5),
                        0.0,
                        Stroke::new(1.0, cursor_color),
                        StrokeKind::Inside,
                    );
                }
                (CursorShape::Block, true) => {
                    painter.rect_filled(cursor_rect, 0.0, cursor_color);
                    if c.ch != ' ' {
                        painter.text(min, egui::Align2::LEFT_TOP, c.ch, font.clone(), color(c.bg));
                    }
                }
                (CursorShape::Underline, true) => {
                    let bar = Rect::from_min_max(
                        pos2(cursor_rect.min.x, cursor_rect.max.y - 2.0),
                        cursor_rect.max,
                    );
                    painter.rect_filled(bar, 0.0, cursor_color);
                }
                (CursorShape::Beam, true) => {
                    let bar = Rect::from_min_size(cursor_rect.min, vec2(2.0, cell.y));
                    painter.rect_filled(bar, 0.0, cursor_color);
                }
            }
        }
    }
}

fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .ok()
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_hit_testing() {
        let m = Metrics {
            cell: vec2(10.0, 20.0),
            origin: pos2(100.0, 50.0),
        };
        assert_eq!(m.cell_at(pos2(100.0, 50.0), 80, 24), (0, 0, false));
        assert_eq!(m.cell_at(pos2(126.0, 95.0), 80, 24), (2, 2, true));
        // Clamped to the grid.
        assert_eq!(m.cell_at(pos2(5000.0, 5000.0), 80, 24), (79, 23, false));
        assert_eq!(m.cell_at(pos2(0.0, 0.0), 80, 24), (0, 0, false));
    }
}
