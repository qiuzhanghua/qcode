//! Drawing the terminal (ADR-0062 §4).
//!
//! Every frame reads the grid once and paints the rows into a single [`canvas`].
//! Nothing here builds an element per cell: a 120×40 grid is ~4800 cells, and a
//! shell printing `ls -l` would otherwise spend the frame constructing elements
//! for characters that are about to be replaced.
//!
//! The grid size comes from the window: the prepaint step measures the canvas and
//! stores the largest grid that fits into [`Shell::terminal_size`], and the next
//! render applies it with `Session::resize`. Keeping `resize` out of the paint
//! callback is what stops the session from being touched while the entity that
//! owns it is being read.

use crate::{
    CloseTerminalShortcut, CopySelectionShortcut, NextTerminalShortcut, PrevTerminalShortcut,
};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Bounds, Context, FontStyle, FontWeight, Hsla, Pixels, SharedString, Styled, TextRun,
    Window, canvas, div, point, prelude::*, px, rgb,
};
use qcode_terminal::View;
use qcode_terminal::alacritty_terminal::term::TermMode;
use qcode_terminal::alacritty_terminal::term::cell::Flags;
use qcode_terminal::alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};

use crate::shell::Shell;

/// The 16 ANSI colours, xterm's defaults. Programs pick these by number, so they
/// have to be the ones every other terminal shows.
/// The terminal draws on its own canvas, not on the app's: a shell's colours are
/// chosen against a dark background, and a program that paints `\x1b[30m` (black)
/// on a white pane is invisible. Same reasoning as the ANSI palette below.
pub(crate) const TERMINAL_BACKGROUND: u32 = 0x1c_1c_21;
pub(crate) const TERMINAL_FOREGROUND: u32 = 0xd6_d6_d6;

const ANSI: [u32; 16] = [
    0x00_00_00, 0xcd_00_00, 0x00_cd_00, 0xcd_cd_00, 0x00_00_ee, 0xcd_00_cd, 0x00_cd_cd, 0xe5_e5_e5,
    0x7f_7f_7f, 0xff_00_00, 0x00_ff_00, 0xff_ff_00, 0x5c_5c_ff, 0xff_00_ff, 0x00_ff_ff, 0xff_ff_ff,
];

struct Metrics {
    font: gpui_kit::Font,
    font_size: Pixels,
    advance: Pixels,
    line_height: Pixels,
}

/// The largest grid that fits in `bounds`, never smaller than a usable terminal.
fn grid_for(bounds: Bounds<Pixels>, metrics: &Metrics) -> (usize, usize) {
    let columns = (bounds.size.width / metrics.advance).floor() as i32;
    let rows = (bounds.size.height / metrics.line_height).floor() as i32;
    (columns.max(20) as usize, rows.max(4) as usize)
}

impl Shell {
    pub(crate) fn render_terminal_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let background = Hsla::from(gpui_kit::rgb(TERMINAL_BACKGROUND));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .track_focus(&self.terminal_focus)
            .key_context("Terminal")
            .on_action(cx.listener(|this, _: &CopySelectionShortcut, _window, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTerminalShortcut, window, cx| {
                this.close_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTerminalShortcut, window, cx| {
                this.cycle_terminal(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevTerminalShortcut, window, cx| {
                this.cycle_terminal(false, window, cx)
            }))
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, _window, cx| {
                    let keystroke = &event.keystroke;
                    if this.terminal_key(keystroke, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .child(self.render_terminal_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .child(self.render_terminal_body(cx)),
            )
    }

    fn render_terminal_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.border;
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let secondary = theme.secondary;

        let mut tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .overflow_hidden();
        for (index, tab) in self.terminals.tabs.iter().enumerate() {
            let active = index == self.terminals.active;
            let label = tab.title();
            tabs = tabs.child(
                div()
                    .id(("terminal-tab", tab.id))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if active {
                        secondary
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .text_color(if active { foreground } else { muted })
                    .child(div().max_w(px(180.0)).overflow_hidden().child(label))
                    .child(
                        div()
                            .id(("terminal-close", tab.id))
                            .px_1()
                            .rounded_sm()
                            .hover(|this| this.bg(muted))
                            .child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_terminal(index, window, cx);
                                this.close_terminal(window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_terminal(index, window, cx);
                    })),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(tabs)
            .child(
                div()
                    .id("terminal-new")
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(|this| this.bg(secondary))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("terminal-back")
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(|this| this.bg(secondary))
                    .child("回到任务")
                    .on_click(cx.listener(|this, _, window, cx| this.show_tasks(window, cx))),
            )
    }

    fn render_terminal_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let secondary = theme.secondary;

        if self.terminals.is_empty() {
            let message = self
                .terminals
                .error
                .clone()
                .unwrap_or_else(|| "还没有终端".to_string());
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(muted)
                .child(div().child(message))
                .child(
                    div()
                        .id("terminal-empty-new")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(secondary)
                        .text_color(foreground)
                        .child("新建终端")
                        .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
                )
                .into_any_element();
        }

        // Snapshot the grid here, not inside the paint callback: painting must not
        // touch the entity that is currently being read, and the reader thread
        // already sets `terminal_dirty` so a repaint follows any new output.
        let background = Hsla::from(gpui_kit::rgb(TERMINAL_BACKGROUND));
        let foreground = Hsla::from(gpui_kit::rgb(TERMINAL_FOREGROUND));
        let frame = self
            .terminals
            .active()
            .map(|tab| {
                tab.session.with_view(|view| {
                    TerminalFrame::capture(
                        view,
                        Hsla::from(gpui_kit::rgb(TERMINAL_FOREGROUND)),
                        Hsla::from(gpui_kit::rgb(TERMINAL_BACKGROUND)),
                    )
                })
            })
            .unwrap_or_default();

        // Remembered for the IME: the candidate window points at the cursor cell.
        self.terminal_cursor.set(frame.cursor);
        let cursor_preedit = self.terminal_preedit.clone();

        let size_cell = self.terminal_size.clone();
        // The IME only delivers text to a registered input handler; registering it
        // here (per frame) is the documented way for an element that is not a text
        // field to receive composed input.
        let input_entity = cx.entity();
        let input_focus = self.terminal_focus.clone();
        let geometry_cell = self.terminal_geometry.clone();
        let selection = self
            .terminal_selection
            .map(|(anchor, focus)| ((anchor.row, anchor.column), (focus.row, focus.column)))
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let font_family = self.mono_font.clone();
        let font_size = px(crate::FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed) as f32);

        let body = canvas(
            move |bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App| {
                let text_system = window.text_system().clone();
                let font = gpui_kit::Font {
                    family: font_family.clone(),
                    features: Default::default(),
                    fallbacks: None,
                    weight: FontWeight::NORMAL,
                    style: FontStyle::Normal,
                };
                // Glyph advance is what the grid is built from. `shape_line` with
                // no runs reports zero width, so ask the metrics directly.
                let font_id = text_system.resolve_font(&font);
                let advance = text_system
                    .advance(font_id, font_size, 'M')
                    .map(|size| size.width)
                    .unwrap_or(font_size * 0.6)
                    .max(font_size * 0.4);
                let metrics = Metrics {
                    font,
                    font_size,
                    advance,
                    line_height: (font_size * 1.4).round(),
                };
                let (columns, rows) = grid_for(bounds, &metrics);
                // Applied on the next tick by `Shell::sync_terminal_size`; doing it
                // here would touch the session from inside the paint.
                size_cell.set(Some((columns, rows)));
                geometry_cell.set(Some(crate::terminal::GridGeometry {
                    origin: bounds.origin,
                    advance: metrics.advance,
                    line_height: metrics.line_height,
                }));
                metrics
            },
            move |bounds: Bounds<Pixels>, metrics: Metrics, window: &mut Window, cx: &mut App| {
                window.handle_input(
                    &input_focus,
                    gpui_kit::ElementInputHandler::new(bounds, input_entity.clone()),
                    cx,
                );
                paint_frame(
                    &frame,
                    bounds,
                    &metrics,
                    PaintStyle {
                        background,
                        foreground,
                        selection,
                        preedit: cursor_preedit,
                    },
                    window,
                    cx,
                );
            },
        )
        .size_full()
        .into_any_element();

        div()
            .size_full()
            .child(body)
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, _window, cx| {
                    this.begin_selection(event.position, cx);
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui_kit::MouseMoveEvent, _window, cx| {
                    // Hover moves arrive too; only a held left button drags.
                    if event.pressed_button == Some(gpui_kit::MouseButton::Left) {
                        this.extend_selection(event.position, cx);
                    }
                }),
            )
            .on_mouse_up(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _: &gpui_kit::MouseUpEvent, _window, cx| {
                    this.end_selection(cx);
                }),
            )
            .on_scroll({
                let entity = cx.entity();
                move |event: &gpui_kit::ScrollEvent, _window, cx| {
                    // A line of history per line of wheel: pixels are divided by a line
                    // height so a trackpad flick and a wheel click scroll alike.
                    let lines = match event.delta {
                        gpui_kit::ScrollDelta::Lines(point) => point.y.round() as i32,
                        gpui_kit::ScrollDelta::Pixels(point) => {
                            (point.y.as_f32() / 20.0).round() as i32
                        }
                    };
                    if lines != 0 {
                        entity.update(cx, |shell, cx| shell.scroll_terminal(lines, cx));
                    }
                }
            })
            .into_any_element()
    }
}

/// Colours and selection for one painted frame.
struct PaintStyle {
    background: Hsla,
    foreground: Hsla,
    selection: Option<((usize, usize), (usize, usize))>,
    /// Text an input method is composing: drawn over the cursor, not in the grid.
    preedit: Option<String>,
}

/// One visible frame, copied out of the terminal.
#[derive(Default)]
struct TerminalFrame {
    rows: Vec<Vec<Run>>,
    cursor: Option<(usize, usize)>,
}

struct Run {
    text: String,
    fg: Hsla,
    bg: Option<Hsla>,
    /// Grid cells this run covers. A wide (CJK) glyph occupies two cells but only
    /// one `char`, so the text cannot be laid out by counting characters.
    cells: usize,
}

impl TerminalFrame {
    fn capture(view: &View<'_>, default_fg: Hsla, default_bg: Hsla) -> Self {
        let columns = view.columns();
        let rows = view.screen_lines();
        let mut lines = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut runs: Vec<Run> = Vec::new();
            for column in 0..columns {
                let Some(cell) = view.cell(row, column) else {
                    break;
                };
                // A wide glyph owns two cells: the second one is a spacer whose
                // text is empty. Drawing it as a space is what put a gap after
                // every Chinese character.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    if let Some(run) = runs.last_mut() {
                        run.cells += 1;
                    }
                    continue;
                }
                let fg = resolve(cell.fg, default_fg);
                let bg = resolve(cell.bg, default_bg);
                let bg = (bg != default_bg).then_some(bg);
                let text = if cell.c.is_empty() {
                    " ".to_string()
                } else {
                    cell.c.clone()
                };
                match runs.last_mut() {
                    Some(run) if run.fg == fg && run.bg == bg => {
                        run.text.push_str(&text);
                        run.cells += 1;
                    }
                    _ => runs.push(Run {
                        text,
                        fg,
                        bg,
                        cells: 1,
                    }),
                }
            }
            lines.push(runs);
        }
        // The cursor is a grid position; scrolling pushes the visible rows down, so
        // a cursor parked in history has to be hidden rather than drawn on top of a
        // scrolled-back screen.
        let offset = view.display_offset() as i32;
        let cursor = view.cursor_visible().then(|| {
            let (line, column) = view.cursor();
            let row = line as i32 + offset;
            (row >= 0 && (row as usize) < rows).then_some((row as usize, column))
        });
        let cursor = cursor.flatten();
        Self {
            rows: lines,
            cursor,
        }
    }
}

fn paint_frame(
    frame: &TerminalFrame,
    bounds: Bounds<Pixels>,
    metrics: &Metrics,
    paint: PaintStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let PaintStyle {
        background,
        foreground,
        selection,
        preedit,
    } = paint;
    let paint_preedit = preedit;
    window.paint_quad(fill(bounds, background));
    let text_system = window.text_system().clone();
    let rows = frame
        .rows
        .len()
        .min((bounds.size.height / metrics.line_height).floor() as usize);
    let columns = frame
        .rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min((bounds.size.width / metrics.advance).floor() as usize);

    for row in 0..rows {
        // Below the text: a selection has to look like a highlight behind the
        // glyphs, not a box drawn over them.
        if let Some(((start_row, start_column), (end_row, end_column))) = selection
            && row >= start_row
            && row <= end_row
        {
            let from = if row == start_row { start_column } else { 0 };
            let to = if row == end_row {
                end_column + 1
            } else {
                columns
            };
            let to = to.min(columns);
            if to > from {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x + metrics.advance * from as f32,
                            bounds.origin.y + metrics.line_height * row as f32,
                        ),
                        gpui_kit::size(metrics.advance * (to - from) as f32, metrics.line_height),
                    ),
                    foreground.opacity(0.22),
                ));
            }
        }

        // A hair of top inset: the shaped line is taller than its glyphs and the
        // first row would otherwise clip its ascenders against the tab bar.
        let y = bounds.origin.y + metrics.line_height * row as f32 + px(2.0);
        let mut column = 0usize;
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();

        for run in &frame.rows[row] {
            let width = run.cells;
            if column >= columns {
                break;
            }
            if let Some(bg) = run.bg {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.origin.x + metrics.advance * column as f32, y),
                        gpui_kit::size(metrics.advance * width as f32, metrics.line_height),
                    ),
                    bg,
                ));
            }
            text.push_str(&run.text);
            runs.push(TextRun {
                len: run.text.len(),
                font: metrics.font.clone(),
                color: run.fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
            column += width;
        }

        if text.trim().is_empty() {
            continue;
        }

        let shaped =
            text_system.shape_line(SharedString::from(text), metrics.font_size, &runs, None);
        let _ = shaped.paint(
            point(bounds.origin.x, y),
            metrics.line_height,
            gpui_kit::TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    if let Some(preedit) = &paint_preedit
        && let Some((line, column)) = frame.cursor
        && line < rows
    {
        let origin = point(
            bounds.origin.x + metrics.advance * column as f32,
            bounds.origin.y + metrics.line_height * line as f32,
        );
        let width = metrics.advance * preedit.chars().count() as f32;
        // The grid has no cell for half a syllable: cover whatever is under the
        // composition and draw the pre-edit text on top of it (ADR-0062 §5).
        window.paint_quad(fill(
            Bounds::new(origin, gpui_kit::size(width, metrics.line_height)),
            paint.background,
        ));
        let run = TextRun {
            len: preedit.len(),
            font: metrics.font.clone(),
            color: paint.foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line_text = window.text_system().shape_line(
            preedit.clone().into(),
            metrics.font_size,
            &[run],
            None,
        );
        let _ = line_text.paint(
            origin,
            metrics.line_height,
            gpui_kit::TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    if let Some((line, column)) = frame.cursor
        && line < rows
        && column < columns
    {
        {
            let origin = point(
                bounds.origin.x + metrics.advance * column as f32,
                bounds.origin.y + metrics.line_height * line as f32,
            );
            window.paint_quad(fill(
                Bounds::new(origin, gpui_kit::size(metrics.advance, metrics.line_height)),
                foreground.opacity(0.7),
            ));
        }
    }
}

fn fill(bounds: Bounds<Pixels>, color: Hsla) -> gpui_kit::PaintQuad {
    gpui_kit::quad(
        bounds,
        gpui_kit::Corners::default(),
        color,
        gpui_kit::Edges::default(),
        gpui_kit::transparent_black(),
        gpui_kit::BorderStyle::default(),
    )
}

#[allow(dead_code)]
fn paint_grid(
    view: &View<'_>,
    bounds: Bounds<Pixels>,
    metrics: &Metrics,
    window: &mut Window,
    cx: &mut App,
) -> usize {
    let columns = view
        .columns()
        .min((bounds.size.width / metrics.advance).floor() as usize);
    let rows = view
        .screen_lines()
        .min((bounds.size.height / metrics.line_height).floor() as usize);
    let text_system = window.text_system().clone();
    let default_fg = cx.theme().foreground;
    let default_bg = cx.theme().background;

    for row in 0..rows {
        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let mut last_fg: Option<Hsla> = None;

        for column in 0..columns {
            let Some(cell) = view.cell(row, column) else {
                break;
            };
            let fg = resolve(cell.fg, default_fg);
            let bg = resolve(cell.bg, default_bg);
            if bg != default_bg {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x + metrics.advance * column as f32,
                            bounds.origin.y + metrics.line_height * row as f32,
                        ),
                        gpui_kit::size(metrics.advance, metrics.line_height),
                    ),
                    bg,
                ));
            }

            let glyph = if cell.c.is_empty() {
                " ".to_string()
            } else {
                cell.c.clone()
            };
            let len = glyph.len();
            text.push_str(&glyph);
            if last_fg == Some(fg) {
                if let Some(run) = runs.last_mut() {
                    run.len += len;
                }
            } else {
                runs.push(TextRun {
                    len,
                    font: metrics.font.clone(),
                    color: fg,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                });
                last_fg = Some(fg);
            }
        }

        if text.trim().is_empty() {
            continue;
        }

        let shaped =
            text_system.shape_line(SharedString::from(text), metrics.font_size, &runs, None);
        let _ = shaped.paint(
            point(
                bounds.origin.x,
                bounds.origin.y + metrics.line_height * row as f32,
            ),
            metrics.line_height,
            gpui_kit::TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    rows
}

fn paint_cursor(
    view: &View<'_>,
    bounds: Bounds<Pixels>,
    metrics: &Metrics,
    window: &mut Window,
    _cx: &mut App,
) {
    if !view.cursor_visible() {
        return;
    }
    let (line, column) = view.cursor();
    if view.cell(line, column).is_none() {
        return;
    }
    let origin = point(
        bounds.origin.x + metrics.advance * column as f32,
        bounds.origin.y + metrics.line_height * line as f32,
    );
    window.paint_quad(fill(
        Bounds::new(origin, gpui_kit::size(metrics.advance, metrics.line_height)),
        Hsla::from(rgb(0x7f7f7f66)),
    ));
}

fn resolve(color: TermColor, fallback: Hsla) -> Hsla {
    match color {
        TermColor::Spec(rgb) => Hsla::from(gpui_kit::rgb(
            ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        )),
        TermColor::Indexed(index) => indexed(index),
        TermColor::Named(named) => named_color(named, fallback),
    }
}

fn named_color(named: NamedColor, fallback: Hsla) -> Hsla {
    use NamedColor::*;
    let index = match named {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        _ => return fallback,
    };
    Hsla::from(rgb(ANSI[index]))
}

fn indexed(index: u8) -> Hsla {
    match index {
        0..=15 => Hsla::from(rgb(ANSI[index as usize])),
        16..=231 => {
            let index = index - 16;
            let steps = [0u32, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let r = steps[(index / 36) as usize];
            let g = steps[((index % 36) / 6) as usize];
            let b = steps[(index % 6) as usize];
            Hsla::from(rgb((r << 16) | (g << 8) | b))
        }
        _ => {
            let level = 8 + (index - 232) as u32 * 10;
            Hsla::from(rgb((level << 16) | (level << 8) | level))
        }
    }
}

/// The `TermMode` a key press has to be encoded for.
pub(crate) fn mode_of(session: &qcode_terminal::Session) -> TermMode {
    session.with_view(|view| view.mode())
}

#[cfg(test)]
mod tests {
    use super::TerminalFrame;

    fn capture_line(text: &str) -> String {
        let mut screen = qcode_terminal::Screen::new(20, 3, 100);
        screen.advance(text.as_bytes());
        let frame = TerminalFrame::capture(
            &screen.view(),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_FOREGROUND)),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_BACKGROUND)),
        );
        frame.rows[0]
            .iter()
            .map(|run| run.text.clone())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn a_wide_glyph_does_not_leave_a_gap_behind_it() {
        // The grid gives a CJK character two cells; the second is a spacer whose
        // text is empty. Rendering it as a space put a hole after every character.
        assert_eq!(capture_line("汉字A\r\n"), "汉字A");
    }

    #[test]
    fn plain_ascii_is_unchanged() {
        assert_eq!(capture_line("echo hi\r\n"), "echo hi");
    }
}
