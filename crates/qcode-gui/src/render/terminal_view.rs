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
    Window, canvas, div, point, prelude::*, px, rgb, rgba,
};
use qcode_terminal::View;
use qcode_terminal::alacritty_terminal::term::TermMode;
use qcode_terminal::alacritty_terminal::term::cell::Flags;
use qcode_terminal::alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};

use crate::shell::Shell;
use crate::sp;

/// Tab label size. Chrome, not content: the pane draws at `COMPOSER_TEXT_SIZE`
/// and the tab is a caption above it, so it follows the same `sp` zoom axis the
/// rest of the window's small text uses.
const TAB_TEXT_SIZE: f32 = 13.0;

/// Widest a tab gets before its title is ellipsized. A shell names its tab after
/// the directory it is in, and those names are long; without a cap two tabs
/// leave no room for the `+`.
const TAB_MAX_WIDTH: f32 = 180.0;

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
    /// Latin/ASCII face: the app's code font.
    font: gpui_kit::Font,
    font_size: Pixels,
    /// The face CJK goes through: the one the composer's text uses.
    cjk_font: gpui_kit::Font,
    /// Size at which one CJK em is exactly two cells. Scaling the face is what lets a
    /// proportional CJK font sit on a cell grid at all: a hanzi is one em, and two cells
    /// of a 0.6 em Latin face are 1.2 em, so the CJK face is drawn 1.2x larger.
    cjk_font_size: Pixels,
    advance: Pixels,
    line_height: Pixels,
    /// Top-left corner of cell (0, 0), already inside the padding and shifted by the
    /// leftover cell space. Painter, hit testing and the IME all position from this —
    /// they used to position from `bounds.origin`, which is why every leftover pixel
    /// piled up at the right/bottom.
    grid_origin: gpui_kit::Point<Pixels>,
}

/// The largest grid that fits in `bounds`, never smaller than a usable terminal.
fn grid_for(bounds: Bounds<Pixels>, metrics: &Metrics) -> (usize, usize) {
    let (columns, rows, _) = grid_layout(bounds, metrics.advance, metrics.line_height);
    (columns, rows)
}

/// How many whole cells fit, and where to start drawing them.
///
/// A terminal grid is made of fixed cells, so a viewport almost never divides evenly:
/// at the default 15 px text the row is 21 px tall, leaving up to 20 px spare, and
/// columns leave up to `advance - 1`. Those leftovers used to stay at the right and
/// bottom, which reads as "the panel is not finished" — and it moved in jumps as the
/// window was resized.
///
/// Splitting them in half puts the same margin on all four sides. The grid stays on
/// whole cells, so nothing blurs; only where it starts changes.
fn grid_layout(
    bounds: Bounds<Pixels>,
    advance: Pixels,
    line_height: Pixels,
) -> (usize, usize, gpui_kit::Point<Pixels>) {
    let advance_f = advance.as_f32().max(1.0);
    let line_f = line_height.as_f32().max(1.0);
    // `max(20)`/`max(4)` keep the terminal usable when the window is tiny; the grid may
    // then be wider than the view, so the offset is clamped at zero rather than going
    // negative (which would draw cell 0 outside the element).
    let columns = ((bounds.size.width.as_f32() / advance_f).floor() as i32).max(20) as usize;
    let rows = ((bounds.size.height.as_f32() / line_f).floor() as i32).max(4) as usize;
    let leftover_x = (bounds.size.width.as_f32() - advance_f * columns as f32).max(0.0);
    let leftover_y = (bounds.size.height.as_f32() - line_f * rows as f32).max(0.0);
    (
        columns,
        rows,
        gpui_kit::point(
            bounds.origin.x + px((leftover_x / 2.0).floor()),
            bounds.origin.y + px((leftover_y / 2.0).floor()),
        ),
    )
}

/// The size the terminal draws text at: the composer's, at the current zoom.
fn terminal_font_size() -> Pixels {
    // Routed through the `_at` form on purpose: the test asserts on that one, so the
    // production path has to be the one under test.
    terminal_font_size_at(crate::FONT_SIZE_PX.load(std::sync::atomic::Ordering::Relaxed))
}

/// Size to draw CJK at so that one em fills exactly two cells.
///
/// Measured rather than assumed: a hanzi is one em in essentially every CJK font, but
/// the *ratio* to the cell depends on the Latin face — a face whose hanzi are narrower
/// (a condensed cut) has to grow more, and the UI font is not chosen for terminal
/// metrics at all.
///
/// Two fallbacks, both chosen so nothing can drift: an unmeasurable or zero-width em
/// draws CJK at the Latin size (two cells may be a little wide for a hanzi, but run
/// positions come from the grid, so the columns still line up), and the result is
/// clamped to [0.5x, 2x] of the Latin size so a nonsense measurement cannot produce an
/// invisible or enormous glyph.
fn cjk_size_for(font_size: Pixels, advance: Pixels, em: Option<Pixels>) -> Pixels {
    let Some(em) = em.filter(|em| *em > px(0.0)) else {
        return font_size;
    };
    (font_size * (advance * 2.0 / em))
        .max(font_size * 0.5)
        .min(font_size * 2.0)
}

/// `terminal_font_size` for an explicit zoom axis, so the relation to the composer can
/// be asserted without touching the global.
fn terminal_font_size_at(axis: u32) -> Pixels {
    crate::composer_text_size_at(axis)
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
            // The body is a panel, not a wash of colour running to the window edge.
            // Only left/right/bottom: the tab bar above already has a `border_b_1`,
            // and a fourth side here would draw that separator twice — two hairlines
            // a few pixels apart read as a mistake, not as depth.
            //
            // The top corners get no radius for the same reason: a rounded corner
            // needs a top edge to turn, and there is none.
            //
            // `rgba(0x7f7f7f20)` rather than `theme.border`: the panel border is a
            // divider between two dark surfaces, not a control outline, so it should
            // be a hint of an edge. The same token is used for the transcript and
            // settings separators.
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .overflow_hidden()
                    .rounded_b(px(PANEL_RADIUS))
                    .border_l_1()
                    .border_r_1()
                    .border_b_1()
                    .border_color(rgba(0x7f_7f_7f_20))
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
            .flex_1()
            .min_w(px(0.0))
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
                    .min_w(px(0.0))
                    .max_w(px(TAB_MAX_WIDTH))
                    .px_2()
                    .py(px(3.0))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if active {
                        secondary
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .text_color(if active { foreground } else { muted })
                    // One line, always. A shell sets its title to the directory it
                    // is in, and left to wrap that path turns the tab bar into a
                    // two- or three-line block (ADR-0062 §4). The label is
                    // `sp(13)` like every other piece of chrome: it is a caption
                    // on the pane, not a heading.
                    .child(
                        div()
                            .text_size(sp(TAB_TEXT_SIZE))
                            .line_height(sp(TAB_TEXT_SIZE + 5.0))
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("terminal-close", tab.id))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded_sm()
                            .text_size(px(12.0))
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
            .py(px(3.0))
            .border_b_1()
            .border_color(border)
            .child(tabs)
            .child(
                div()
                    .id("terminal-new")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(20.0))
                    .h(px(20.0))
                    .rounded_sm()
                    .text_size(sp(TAB_TEXT_SIZE))
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(|this| this.bg(secondary))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
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
        // Two faces, because the code font has no hanzi and a CJK mono face has a
        // Latin design nobody chose. See `pick_terminal_fonts`.
        let font_cell = self.terminal_font.clone();
        let cjk_font_cell = self.terminal_cjk_font.clone();
        let configured_font = self.terminal_config().font;
        let preferred_font = self.mono_font.clone();
        // The composer's size, not the raw zoom axis: the two views have to match
        // (see `COMPOSER_TEXT_SIZE`).
        let font_size = terminal_font_size();

        let body = canvas(
            move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                let text_system = window.text_system().clone();
                let ui_family = {
                    use gpui_kit::component::ActiveTheme as _;
                    cx.theme().font_family.clone()
                };
                let (latin_family, cjk_family) = resolve_terminal_fonts(
                    &font_cell,
                    &cjk_font_cell,
                    &configured_font,
                    &preferred_font,
                    &ui_family,
                );
                let face = |family: gpui_kit::SharedString| gpui_kit::Font {
                    family,
                    features: Default::default(),
                    fallbacks: None,
                    weight: FontWeight::NORMAL,
                    style: FontStyle::Normal,
                };
                let font = face(latin_family);
                let cjk_font = face(cjk_family);
                // Glyph advance is what the grid is built from. `shape_line` with
                // no runs reports zero width, so ask the metrics directly.
                let font_id = text_system.resolve_font(&font);
                let advance = text_system
                    .advance(font_id, font_size, 'M')
                    .map(|size| size.width)
                    .unwrap_or(font_size * 0.6)
                    .max(font_size * 0.4);
                // The CJK face is scaled so one em spans exactly two cells; the
                // measurement is done here, the arithmetic in `cjk_size_for` so it can be
                // tested without a text system.
                let cjk_font_id = text_system.resolve_font(&cjk_font);
                let cjk_em = text_system
                    .advance(cjk_font_id, font_size, '中')
                    .map(|size| size.width)
                    .ok();
                let cjk_font_size = cjk_size_for(font_size, advance, cjk_em);
                let metrics = Metrics {
                    font,
                    font_size,
                    cjk_font,
                    cjk_font_size,
                    advance,
                    line_height: (font_size * 1.4).round(),
                    // Filled in below, once the bounds are known.
                    grid_origin: bounds.origin,
                };
                let (columns, rows, grid_origin) =
                    grid_layout(bounds, metrics.advance, metrics.line_height);
                // Applied on the next tick by `Shell::sync_terminal_size`; doing it
                // here would touch the session from inside the paint.
                size_cell.set(Some((columns, rows)));
                // One origin for everything that has to agree: the painter draws from
                // it, `terminal_point` turns a click into a cell by subtracting it, and
                // `bounds_for_range` places the IME candidate window from it. Storing
                // the padded-and-centred corner here is what keeps those three from
                // drifting apart.
                geometry_cell.set(Some(crate::terminal::GridGeometry {
                    origin: grid_origin,
                    advance: metrics.advance,
                    line_height: metrics.line_height,
                }));
                Metrics {
                    grid_origin,
                    ..metrics
                }
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

        // A terminal with no margin puts the first and last row/column against
        // the edge of the view, where they read as cut off. The padding also
        // gives the cursor block somewhere to sit on the last line.
        div()
            .size_full()
            .bg(background)
            .px(px(TERMINAL_PADDING_X))
            .py(px(TERMINAL_PADDING_Y))
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

/// Breathing room between the grid and the edge of the view.
/// Corner radius of the terminal body panel below the tab bar.
pub(crate) const PANEL_RADIUS: f32 = 8.0;

pub(crate) const TERMINAL_PADDING_X: f32 = 12.0;
pub(crate) const TERMINAL_PADDING_Y: f32 = 8.0;

/// Colours and selection for one painted frame.
struct PaintStyle {
    background: Hsla,
    foreground: Hsla,
    selection: Option<((usize, usize), (usize, usize))>,
    /// Text an input method is composing: drawn over the cursor, not in the grid.
    preedit: Option<String>,
}

/// The families the terminal should draw with.
///
/// Two families, because one cannot do both jobs well:
///
/// - **Latin** uses the app's code font (`theme.mono_font_family`, the same one every
///   other code surface uses, configurable as `[gui] code_font`). A terminal is a code
///   surface; borrowing a CJK face's Latin design just because that face happens to be
///   monospaced is what made the terminal look unlike the rest of the window.
/// - **CJK** uses the UI font (`theme.font_family`) — the face the composer's text goes
///   through. Hanzi are what the user reads in both places, so they should come from the
///   same family rather than from a terminal-only CJK mono face.
///
/// This is possible because runs are painted at their column offset with a size chosen
/// per run (see `paint_frame`): the CJK face does not have to be a 1:2 *monospace* font,
/// only to be scaled so one em is exactly two cells. Without that, mixing families
/// would drift off the grid — each hanzi is 1 em, while two cells of a 0.6 em Latin face
/// are 1.2 em.
///
/// An explicit `[terminal] font` overrides both: a user who named a family wants that
/// family everywhere, including for the Latin it was chosen for.
fn pick_terminal_fonts(
    configured: &str,
    mono_family: &gpui_kit::SharedString,
    ui_family: &gpui_kit::SharedString,
) -> (gpui_kit::SharedString, gpui_kit::SharedString) {
    let configured = configured.trim();
    if !configured.is_empty() {
        let family: gpui_kit::SharedString = configured.to_string().into();
        return (family.clone(), family);
    }
    (mono_family.clone(), ui_family.clone())
}

/// Pick the two faces once and remember them, or hand back what was remembered.
///
/// Takes the cache cells by reference instead of reading fields off `Shell`, so the
/// borrow discipline below can be tested without a `Window` — the panic it used to
/// cause was "RefCell already borrowed" and had nothing to do with GPUI.
fn resolve_terminal_fonts(
    latin_cell: &std::cell::RefCell<Option<gpui_kit::SharedString>>,
    cjk_cell: &std::cell::RefCell<Option<gpui_kit::SharedString>>,
    configured: &str,
    mono_family: &gpui_kit::SharedString,
    ui_family: &gpui_kit::SharedString,
) -> (gpui_kit::SharedString, gpui_kit::SharedString) {
    // The reads finish before the write: `match latin_cell.borrow().clone()` would keep
    // the shared borrow alive until the end of the `match` (a scrutinee's temporaries
    // are not dropped early) and then `borrow_mut` in an arm would panic. That is a
    // first-paint panic, because "nothing cached yet" *is* the first paint.
    let cached_latin = latin_cell.borrow().clone();
    let cached_cjk = cjk_cell.borrow().clone();
    match (cached_latin, cached_cjk) {
        (Some(latin), Some(cjk)) => (latin, cjk),
        // Only the Latin half cached: a theme change between the two writes. Pick
        // again rather than pair a stale family with a fresh one.
        (None, _) | (Some(_), None) => {
            let (latin, cjk) = pick_terminal_fonts(configured, mono_family, ui_family);
            *latin_cell.borrow_mut() = Some(latin.clone());
            *cjk_cell.borrow_mut() = Some(cjk.clone());
            (latin, cjk)
        }
    }
}

/// One visible frame, copied out of the terminal.
#[derive(Default)]
struct TerminalFrame {
    rows: Vec<Vec<Run>>,
    cursor: Option<(usize, usize)>,
}

struct Run {
    /// First grid column this run covers. Runs are painted at their column rather
    /// than laid out one after another, because a run of hanzi and a run of ASCII are
    /// drawn with different faces and sizes — their widths cannot be trusted to add up.
    column: usize,
    text: String,
    fg: Hsla,
    bg: Option<Hsla>,
    /// Grid cells this run covers. A wide (CJK) glyph occupies two cells but only
    /// one `char`, so the text cannot be laid out by counting characters.
    cells: usize,
    /// Whether the glyphs in this run are wide (CJK/fullwidth), which decides the face
    /// and the size used to paint it.
    wide: bool,
}

impl TerminalFrame {
    fn capture(view: &View<'_>, default_fg: Hsla, default_bg: Hsla) -> Self {
        let columns = view.columns();
        let rows = view.screen_lines();
        // One pass over the grid. `View::cell` scans the whole grid per lookup,
        // so asking it for 114×33 cells turned every frame into ~10⁷ comparisons
        // — visible as a lagging echo.
        let mut lines: Vec<Vec<Run>> = (0..rows).map(|_| Vec::new()).collect();
        for (row, column, cell) in view.cells() {
            if row >= rows || column >= columns {
                continue;
            }
            let runs = &mut lines[row];
            // A wide glyph owns two cells: the second one is a spacer whose text
            // is empty. Drawing it as a space is what put a gap after every
            // Chinese character.
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
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            match runs.last_mut() {
                // A run never mixes wide and narrow cells: the two are painted with
                // different faces at different sizes, so they cannot share a run.
                Some(run) if run.fg == fg && run.bg == bg && run.wide == wide => {
                    run.text.push_str(&text);
                    run.cells += 1;
                }
                _ => runs.push(Run {
                    column,
                    text,
                    fg,
                    bg,
                    cells: 1,
                    wide,
                }),
            }
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
    // Cells, not runs: `Vec::len` counted the coloured segments and handed the
    // selection a width far narrower than the row.
    let columns = frame
        .rows
        .iter()
        .map(|runs| runs.iter().map(|run| run.cells).sum::<usize>())
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
                            metrics.grid_origin.x + metrics.advance * from as f32,
                            metrics.grid_origin.y + metrics.line_height * row as f32,
                        ),
                        gpui_kit::size(metrics.advance * (to - from) as f32, metrics.line_height),
                    ),
                    foreground.opacity(0.22),
                ));
            }
        }

        // A hair of top inset: the shaped line is taller than its glyphs and the
        // first row would otherwise clip its ascenders against the tab bar.
        let y = metrics.grid_origin.y + metrics.line_height * row as f32 + px(2.0);

        // One shaped line per run, painted at the run's own column.
        //
        // The alternative — one line per row with a `TextRun` per colour — needs every
        // glyph on the row to come from one face at one size. That is what forced the
        // whole terminal onto a CJK monospace face before: a code font's ASCII is 0.6 em
        // while a hanzi is 1 em, so two cells and one hanzi disagreed by 0.2 em and the
        // line drifted. Positioning by column removes the disagreement, and each face is
        // then free to be the best one for its script. Cost: no shaping across runs, so a
        // ligature spanning two colours is not formed (they do not occur in practice).
        //
        // The unit is the run, not the cell: `capture` merges neighbours that share a
        // colour and script, so a coloured `ls` is a handful of shaped lines per row, not
        // one per character. `runs_tile_the_row_without_gaps` pins the merging down,
        // because if it ever regressed the frame would go from ~8 shapes a row to 80.
        for run in &frame.rows[row] {
            if run.column >= columns {
                break;
            }
            let x = metrics.grid_origin.x + metrics.advance * run.column as f32;
            let width = run.cells.min(columns - run.column);
            if let Some(bg) = run.bg {
                window.paint_quad(fill(
                    Bounds::new(
                        point(x, y),
                        gpui_kit::size(metrics.advance * width as f32, metrics.line_height),
                    ),
                    bg,
                ));
            }
            // Spaces need no glyphs: the background above is the whole of them, and
            // shaping runs of blanks per row is pure cost.
            if run.text.trim().is_empty() {
                continue;
            }

            let (font, font_size) = if run.wide {
                (metrics.cjk_font.clone(), metrics.cjk_font_size)
            } else {
                (metrics.font.clone(), metrics.font_size)
            };
            let text_run = TextRun {
                len: run.text.len(),
                font,
                color: run.fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = text_system.shape_line(
                SharedString::from(run.text.clone()),
                font_size,
                &[text_run],
                None,
            );
            let _ = shaped.paint(
                point(x, y),
                metrics.line_height,
                gpui_kit::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    if let Some(preedit) = &paint_preedit
        && let Some((line, column)) = frame.cursor
        && line < rows
    {
        let origin = point(
            metrics.grid_origin.x + metrics.advance * column as f32,
            metrics.grid_origin.y + metrics.line_height * line as f32,
        );
        // A composing syllable is Chinese: count it in cells (two per hanzi), not in
        // chars, or the highlight is half as wide as the text it covers.
        let cells: usize = preedit
            .chars()
            .map(|c| if c.is_ascii() { 1 } else { 2 })
            .sum();
        let width = metrics.advance * cells as f32;
        // The grid has no cell for half a syllable: cover whatever is under the
        // composition and draw the pre-edit text on top of it (ADR-0062 §5).
        window.paint_quad(fill(
            Bounds::new(origin, gpui_kit::size(width, metrics.line_height)),
            paint.background,
        ));
        // Same face rule as the grid: the pre-edit is usually hanzi, so draw it with
        // the CJK face at the size that fills cells — otherwise the composition looks
        // unlike what committing it will look like.
        let cjk = !preedit.is_ascii();
        let run = TextRun {
            len: preedit.len(),
            font: if cjk {
                metrics.cjk_font.clone()
            } else {
                metrics.font.clone()
            },
            color: paint.foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let preedit_size = if cjk {
            metrics.cjk_font_size
        } else {
            metrics.font_size
        };
        let line_text =
            window
                .text_system()
                .shape_line(preedit.clone().into(), preedit_size, &[run], None);
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
                metrics.grid_origin.x + metrics.advance * column as f32,
                metrics.grid_origin.y + metrics.line_height * line as f32,
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
    use gpui_kit::px;

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
    #[ignore]
    fn capture_cost() {
        let mut screen = qcode_terminal::Screen::new(120, 40, 1000);
        for i in 0..2000 {
            screen.advance(format!("line {i} with some text\r\n").as_bytes());
        }
        let fg = gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_FOREGROUND));
        let bg = gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_BACKGROUND));
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let _ = TerminalFrame::capture(&screen.view(), fg, bg);
        }
        // A frame must cost much less than the 16ms tick; run with `--ignored`
        // when touching the capture path.
        eprintln!("capture 100 frames: {:?}", start.elapsed());
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

    #[test]
    fn a_cjk_run_and_a_latin_run_are_never_mixed() {
        // The two are painted with different faces at different sizes, positioned by
        // column, so a run that mixed them could not be drawn correctly at all: one face
        // or the other would be wrong for half the glyphs.
        let mut screen = qcode_terminal::Screen::new(20, 3, 100);
        screen.advance("a汉b字c\r\n".as_bytes());
        let frame = TerminalFrame::capture(
            &screen.view(),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_FOREGROUND)),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_BACKGROUND)),
        );
        let runs = &frame.rows[0];
        for run in runs {
            let has_wide = !run.text.is_ascii();
            assert_eq!(
                has_wide, run.wide,
                "run {:?} has wide={} but contains wide glyphs={}",
                run.text, run.wide, has_wide
            );
        }
        // And the text still reads in order, with the columns adding up.
        let text: String = runs.iter().map(|run| run.text.clone()).collect();
        assert_eq!(text.trim_end(), "a汉b字c");
    }

    #[test]
    fn every_run_sits_at_the_column_its_glyphs_occupy() {
        // This is what replaced "let the shaper decide where the next run goes": each
        // run paints at `advance * column`, so the columns have to be right or the line
        // drifts even though nothing is measured at paint time.
        let mut screen = qcode_terminal::Screen::new(20, 3, 100);
        screen.advance("a汉b\r\n".as_bytes());
        let frame = TerminalFrame::capture(
            &screen.view(),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_FOREGROUND)),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_BACKGROUND)),
        );
        let columns: Vec<usize> = frame.rows[0].iter().map(|run| run.column).collect();
        // "a" at column 0, "汉" at column 1, "b" at column 3 — the hanzi consumed two
        // cells, so the next glyph starts two columns on, not one.
        assert_eq!(columns, vec![0, 1, 3]);
        // The wide run folded in its spacer cell, so it covers exactly two cells.
        let wide = frame.rows[0]
            .iter()
            .find(|run| run.wide)
            .expect("a wide run");
        assert_eq!(wide.cells, 2);
        assert_eq!(wide.text, "汉");
    }

    #[test]
    fn the_code_font_is_preferred_over_a_cjk_monospace_face() {
        // The regression this replaces: the terminal used to take the first installed
        // CJK monospace family for *everything*, so its ASCII came from a face nobody
        // chose and looked unlike the rest of the window. Latin now follows the code
        // font, and CJK follows the composer's face.
        let mono: gpui_kit::SharedString = "JetBrains Mono".into();
        let ui: gpui_kit::SharedString = ".SystemUIFont".into();
        let (latin, cjk) = super::pick_terminal_fonts("", &mono, &ui);
        assert_eq!(latin, mono);
        assert_eq!(cjk, ui);
    }

    #[test]
    fn the_first_paint_does_not_panic_on_the_font_cache() {
        // Regression: this used to panic with "RefCell already borrowed" on the very
        // first paint of every terminal — the click that opens 终端. The reads have to
        // finish before the cache is written, and "nothing cached yet" is by definition
        // the first paint, so the bug hit every user immediately.
        let latin: std::cell::RefCell<Option<gpui_kit::SharedString>> = Default::default();
        let cjk: std::cell::RefCell<Option<gpui_kit::SharedString>> = Default::default();
        let mono: gpui_kit::SharedString = "JetBrains Mono".into();
        let ui: gpui_kit::SharedString = ".SystemUIFont".into();

        let picked = super::resolve_terminal_fonts(&latin, &cjk, "", &mono, &ui);

        assert_eq!(picked.0, mono);
        assert_eq!(picked.1, ui);
        // And the cache was filled, so the pick happens once and not per frame.
        assert_eq!(latin.borrow().as_ref(), Some(&picked.0));
        assert_eq!(cjk.borrow().as_ref(), Some(&picked.1));
    }

    #[test]
    fn a_second_paint_reuses_the_cached_faces() {
        let latin: std::cell::RefCell<Option<gpui_kit::SharedString>> = Default::default();
        let cjk: std::cell::RefCell<Option<gpui_kit::SharedString>> = Default::default();
        let mono: gpui_kit::SharedString = "JetBrains Mono".into();
        let ui: gpui_kit::SharedString = ".SystemUIFont".into();
        let first = super::resolve_terminal_fonts(&latin, &cjk, "", &mono, &ui);

        // A different theme on the second call must not be consulted: the faces are
        // picked once, and re-picking per frame would also mean re-resolving fonts.
        let other: gpui_kit::SharedString = "Something Else".into();
        let second = super::resolve_terminal_fonts(&latin, &cjk, "", &other, &other);
        assert_eq!(first, second);
    }

    #[test]
    fn a_half_filled_cache_is_refilled_rather_than_trusted() {
        // A theme change between the two writes can leave the Latin family cached and
        // the CJK one not. Pairing a stale family with a fresh one would draw two
        // scripts from two different themes.
        let latin: std::cell::RefCell<Option<gpui_kit::SharedString>> =
            std::cell::RefCell::new(Some("Stale".into()));
        let cjk: std::cell::RefCell<Option<gpui_kit::SharedString>> = Default::default();
        let mono: gpui_kit::SharedString = "JetBrains Mono".into();
        let ui: gpui_kit::SharedString = ".SystemUIFont".into();

        let picked = super::resolve_terminal_fonts(&latin, &cjk, "", &mono, &ui);
        assert_eq!(picked.0, mono);
        assert_eq!(picked.1, ui);
    }

    #[test]
    fn leftover_cell_space_is_split_evenly_instead_of_piling_up_at_the_bottom() {
        // 800 px tall, 21 px rows: 38 rows use 798 px, so 2 px are spare. Both used to
        // stay below the last row, which is what made the bottom margin look wrong and
        // bigger than the top one (and it jumped by up to 20 px when the window moved).
        let bounds = gpui_kit::Bounds::new(
            gpui_kit::point(px(0.0), px(0.0)),
            gpui_kit::size(px(1200.0), px(800.0)),
        );
        let (columns, rows, origin) = super::grid_layout(bounds, px(9.0), px(21.0));

        assert_eq!(rows, 38);
        let spare_y = 800.0 - 21.0 * rows as f32;
        assert_eq!(origin.y, px((spare_y / 2.0).floor()));

        let spare_x = 1200.0 - 9.0 * columns as f32;
        assert_eq!(origin.x, px((spare_x / 2.0).floor()));

        // The point of the change: the gap above the first row equals the gap after the
        // last one, to within a pixel (an odd leftover cannot be split in half).
        let above = origin.y.as_f32();
        let below = spare_y - above;
        assert!(
            (above - below).abs() <= 1.0,
            "top gap {above} vs bottom gap {below}"
        );
    }

    #[test]
    fn a_tiny_view_does_not_push_the_grid_out_of_the_element() {
        // `max(20)`/`max(4)` make the grid wider/taller than the view when the window is
        // small. The leftover is then negative, and a negative offset would draw cell 0
        // outside the element — clicks there would map to no cell at all.
        let bounds = gpui_kit::Bounds::new(
            gpui_kit::point(px(40.0), px(30.0)),
            gpui_kit::size(px(100.0), px(50.0)),
        );
        let (columns, rows, origin) = super::grid_layout(bounds, px(9.0), px(21.0));
        assert_eq!((columns, rows), (20, 4));
        assert_eq!(origin, gpui_kit::point(px(40.0), px(30.0)));
    }

    #[test]
    fn a_click_maps_back_to_the_cell_under_it() {
        // The regression this guards: prepaint, painting and hit testing have to use
        // one origin. Shifting the drawn grid without shifting this arithmetic would
        // put every click one or two cells off — and selecting text would follow.
        let bounds = gpui_kit::Bounds::new(
            gpui_kit::point(px(12.0), px(8.0)),
            gpui_kit::size(px(1200.0), px(800.0)),
        );
        let (_, _, origin) = super::grid_layout(bounds, px(9.0), px(21.0));

        let geometry = crate::terminal::GridGeometry {
            origin,
            advance: px(9.0),
            line_height: px(21.0),
        };
        // Aim at the middle of cell (3, 5) and expect exactly that cell back.
        let aim = gpui_kit::point(
            geometry.origin.x + geometry.advance * 3.0 + px(4.0),
            geometry.origin.y + geometry.line_height * 5.0 + px(10.0),
        );
        let as_grid = |position: gpui_kit::Point<gpui_kit::Pixels>| {
            let x = (position.x - geometry.origin.x).as_f32();
            let y = (position.y - geometry.origin.y).as_f32();
            (
                (y / geometry.line_height.as_f32()).floor() as usize,
                (x / geometry.advance.as_f32()).floor() as usize,
            )
        };
        assert_eq!(as_grid(aim), (5, 3));
    }

    #[test]
    fn runs_tile_the_row_without_gaps() {
        // Runs are positioned by `advance * column`, so a gap or an overlap between two
        // runs is a shifted glyph. Tiling is the invariant that makes the positioning
        // safe, and it is cheap to check — unlike the pixels, which no test here sees.
        let mut screen = qcode_terminal::Screen::new(40, 3, 100);
        // Colour changes, a wide glyph, and spaces: the three things that split a run.
        screen.advance(
            b"\x1b[31mred\x1b[32mgreen\x1b[0m \x1b[1m\xe6\xb1\x89\xe5\xad\x97\x1b[0m tail\r\n",
        );
        let frame = TerminalFrame::capture(
            &screen.view(),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_FOREGROUND)),
            gpui_kit::Hsla::from(gpui_kit::rgb(super::TERMINAL_BACKGROUND)),
        );

        let mut next_column = 0;
        for run in &frame.rows[0] {
            assert_eq!(
                run.column, next_column,
                "run {:?} starts at {} but the previous one ended at {}",
                run.text, run.column, next_column
            );
            assert!(run.cells > 0, "empty run {:?}", run.text);
            next_column += run.cells;
        }
        // And the text survives, in order.
        let text: String = frame.rows[0].iter().map(|run| run.text.clone()).collect();
        assert_eq!(text.trim_end(), "redgreen 汉字 tail");
    }

    #[test]
    fn a_hanzi_is_drawn_two_cells_wide() {
        // 15 px Latin with a 9 px cell: two cells are 18 px, so a hanzi whose em is
        // 15 px has to be drawn at 18 px to fill them. Without this the cell grid and
        // the glyph disagree and a line of Chinese ends short of where it should.
        let size = super::cjk_size_for(px(15.0), px(9.0), Some(px(15.0)));
        assert_eq!(size, px(18.0));
        // A narrower CJK face grows more: same two cells, smaller em.
        let size = super::cjk_size_for(px(15.0), px(9.0), Some(px(12.0)));
        assert_eq!(size, px(22.5));
    }

    #[test]
    fn an_unmeasurable_cjk_face_does_not_divide_by_zero() {
        // `advance('中')` fails when the family has no hanzi at all, which is exactly the
        // case for a terminal font: the measurement must degrade, not panic.
        for em in [None, Some(px(0.0))] {
            assert_eq!(
                super::cjk_size_for(px(15.0), px(9.0), em),
                px(15.0),
                "em={em:?} should fall back to the Latin size"
            );
        }
    }

    #[test]
    fn an_absurd_cjk_measurement_cannot_make_a_glyph_huge_or_invisible() {
        // A font that reports a 1 px em, or a 1000 px one, must not scale the text off
        // the screen: the result stays within half to double the Latin size.
        let tiny = super::cjk_size_for(px(15.0), px(9.0), Some(px(0.5)));
        assert_eq!(tiny, px(30.0), "clamped to 2x");
        let huge = super::cjk_size_for(px(15.0), px(9.0), Some(px(1000.0)));
        assert_eq!(huge, px(7.5), "clamped to 0.5x");
    }

    #[test]
    fn the_terminal_draws_at_the_composers_size() {
        // The shell's output and the text you type about it sit inches apart. At
        // different sizes the terminal reads as a different application pasted into the
        // window — which is what happened while the terminal drew at the raw zoom axis
        // (14 px out of the box) and the composer drew 15·(zoom/14).
        //
        // Against the composer's *design* size, at three zoom steps: 15 design units on a
        // 14 px baseline, so 15 px at 1x, and the ratio holds as the user zooms.
        let baseline = crate::gui_prefs::DEFAULT_FONT_SIZE;
        assert_eq!(
            super::terminal_font_size_at(baseline),
            px(crate::COMPOSER_TEXT_SIZE)
        );
        assert_eq!(super::terminal_font_size_at(baseline * 2), px(30.0));
        // And it must not be the raw axis, which is the bug this replaces.
        assert_ne!(
            super::terminal_font_size_at(baseline),
            px(baseline as f32),
            "the terminal is drawing at the zoom axis instead of the composer's size"
        );
    }

    #[test]
    fn a_configured_family_wins_for_both_scripts() {
        // `[terminal] font` names one family; someone who typed a name wants it used,
        // not quietly split in two.
        let mono: gpui_kit::SharedString = "JetBrains Mono".into();
        let ui: gpui_kit::SharedString = ".SystemUIFont".into();
        let (latin, cjk) = super::pick_terminal_fonts("  Sarasa Term SC ", &mono, &ui);
        assert_eq!(latin, "Sarasa Term SC");
        assert_eq!(cjk, "Sarasa Term SC");
    }
}
