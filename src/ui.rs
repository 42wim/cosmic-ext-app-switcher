// SPDX-License-Identifier: GPL-3.0-only

//! Panel layout and rendering for the window switcher.

use std::ops::Range;

use cosmic::{
    Element,
    iced::{
        Alignment, Background, Border, Color, ContentFit, Length, Shadow, Vector,
        core::text::Wrapping, widget::container::Style as ContainerStyle,
    },
    widget::{column, container, icon, image, mouse_area, responsive, row, space, text},
};

use crate::{
    app::{AppSwitcher, Message},
    icons::{AppVisual, window_label},
};

/// Space between the screen edge and the strip.
const SCREEN_MARGIN: f32 = 48.0;
/// Breathing room around the strip, so its shadow isn't clipped by the surface
/// edge.  The layer surface is only panel-sized because a full-output surface
/// costs a full-output draw/present on every show.
pub const SURFACE_MARGIN: f32 = 40.0;
/// Padding inside the strip container.
const STRIP_PAD: f32 = 20.0;
/// Space between preview cards in a row.
const CARD_GAP: f32 = 12.0;
/// Space between rows.
const ROW_GAP: f32 = 12.0;
/// Padding inside a card, between its border and the preview.
const CARD_PAD: f32 = 6.0;
/// Border width of every card.  Kept constant so selection does not shift the
/// layout; only the colour changes.
const CARD_BORDER: f32 = 2.0;
const MIN_CARD_W: f32 = 170.0;
const MAX_CARD_W: f32 = 400.0;
const MAX_COLS: usize = 6;
const MAX_ROWS: usize = 3;
const PREVIEW_ASPECT: f32 = 9.0 / 16.0;
/// Height of the strip above the grid that holds the page counter.
const COUNTER_BAR: f32 = 18.0;
const COUNTER_SIZE: u16 = 13;
/// Font size of the selected-window label.
const LABEL_SIZE: u16 = 13;
/// Icon size in the selected-window label.
const LABEL_ICON: u16 = 18;
/// Gap between the label icon and its title.
const LABEL_GAP: f32 = 6.0;
/// Horizontal padding inside the label pill.
const LABEL_PAD: f32 = 8.0;
/// Vertical padding inside the label pill.
const LABEL_PAD_Y: f32 = 4.0;
/// Height of one line of label text.
const LABEL_LINE_H: f32 = 16.0;
/// Height of two lines of label text.
const LABEL_TEXT_TWO_H: f32 = 32.0;
/// Height of three lines of label text.
const LABEL_TEXT_THREE_H: f32 = 48.0;
/// Gap between the selected card and its label.
const LABEL_GAP_Y: f32 = 2.0;
/// Height of a three-line label pill.
const LABEL_HEIGHT: f32 = LABEL_TEXT_THREE_H + 2.0 * LABEL_PAD_Y;
/// Space below the grid reserved for a last-row label.
const LABEL_BAND: f32 = LABEL_GAP_Y + LABEL_HEIGHT;

/// Maximum number of preview cards the grid can show at once.
pub const MAX_PREVIEWS: usize = crate::events::MAX_PREVIEWS;

#[derive(Debug, Clone, Copy)]
struct CardMetrics {
    preview_w: f32,
    preview_h: f32,
    card_h: f32,
}

impl CardMetrics {
    fn from_cell_w(cell_w: f32) -> Self {
        let preview_w = (cell_w - 2.0 * (CARD_PAD + CARD_BORDER)).max(1.0);
        let preview_h = (preview_w * PREVIEW_ASPECT).round().max(1.0);
        let card_h = preview_h + 2.0 * (CARD_PAD + CARD_BORDER);
        Self {
            preview_w,
            preview_h,
            card_h,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FullLayout {
    rows: usize,
    cols: usize,
    cell_w: f32,
    grid_w: f32,
    grid_h: f32,
}

impl FullLayout {
    /// The largest grid the screen can hold.  This is the visual target for
    /// every preview count: fewer cards are laid out inside the same area and
    /// become larger, while the strip stays the same size.
    fn compute(screen_w: f32, screen_h: f32) -> Self {
        let available_w = (screen_w - 2.0 * (SCREEN_MARGIN + STRIP_PAD)).max(MIN_CARD_W);
        let available_h = (screen_h - 2.0 * (SCREEN_MARGIN + STRIP_PAD) - COUNTER_BAR - LABEL_BAND)
            .max(MIN_CARD_W * PREVIEW_ASPECT);

        let max_cols_by_width =
            ((available_w + CARD_GAP) / (MIN_CARD_W + CARD_GAP)).floor() as usize;
        let max_cols = max_cols_by_width.clamp(1, MAX_COLS);

        let mut rows = MAX_PREVIEWS.div_ceil(max_cols).clamp(1, MAX_ROWS);

        let (cell_w, cols) = loop {
            let cols = MAX_PREVIEWS
                .min(rows * max_cols)
                .div_ceil(rows)
                .min(max_cols)
                .max(1);

            let cell_by_width =
                (available_w - (cols.saturating_sub(1)) as f32 * CARD_GAP) / cols as f32;
            let card_h_available =
                (available_h - (rows.saturating_sub(1)) as f32 * ROW_GAP) / rows as f32;
            let cell_by_height = ((card_h_available - 2.0 * (CARD_PAD + CARD_BORDER)).max(1.0)
                / PREVIEW_ASPECT)
                + 2.0 * (CARD_PAD + CARD_BORDER);

            let cell_w = cell_by_width.min(cell_by_height).clamp(120.0, MAX_CARD_W);

            if cell_w >= MIN_CARD_W || rows == 1 {
                break (cell_w, cols);
            }
            rows -= 1;
        };

        let CardMetrics { card_h, .. } = CardMetrics::from_cell_w(cell_w);
        let grid_w = cols as f32 * cell_w + (cols.saturating_sub(1)) as f32 * CARD_GAP;
        let grid_h = rows as f32 * card_h + (rows.saturating_sub(1)) as f32 * ROW_GAP;

        Self {
            rows,
            cols,
            cell_w,
            grid_w,
            grid_h,
        }
    }

    fn capacity(&self) -> usize {
        self.rows * self.cols
    }
}

#[derive(Debug, Clone)]
struct Layout {
    /// Outer width of one card, including padding and border.
    cell_w: f32,
    /// Width of the preview image inside a card.
    preview_w: f32,
    /// Height of the preview image inside a card.
    preview_h: f32,
    /// Outer height of one card.
    card_h: f32,
    rows: usize,
    cols: usize,
    /// The slice of windows currently rendered.
    visible: Range<usize>,
    /// Width of the preview grid, excluding strip padding.
    grid_w: f32,
    /// Height of the preview grid, excluding the counter/label bands and strip padding.
    grid_h: f32,
    /// Actual strip content height, excluding strip padding.
    strip_h: f32,
    /// Width of the 6x3 panel, excluding strip padding.
    panel_w: f32,
    /// Height of the 6x3 panel content, including the counter/label bands.
    panel_h: f32,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    cols: usize,
    rows: usize,
    cell_w: f32,
    preview_w: f32,
    preview_h: f32,
    card_h: f32,
    grid_w: f32,
    grid_h: f32,
    orphan: bool,
}

impl Candidate {
    fn new(cols: usize, rows: usize, cell_w: f32, visible_count: usize) -> Self {
        let CardMetrics {
            preview_w,
            preview_h,
            card_h,
        } = CardMetrics::from_cell_w(cell_w);
        let full_rows = visible_count / cols;
        let last_row = visible_count % cols;
        let widest_row = if full_rows > 0 { cols } else { last_row };
        let grid_w = widest_row as f32 * cell_w + (widest_row.saturating_sub(1)) as f32 * CARD_GAP;
        let grid_h = rows as f32 * card_h + (rows.saturating_sub(1)) as f32 * ROW_GAP;

        Self {
            cols,
            rows,
            cell_w,
            preview_w,
            preview_h,
            card_h,
            grid_w,
            grid_h,
            orphan: visible_count % cols == 1,
        }
    }
}

fn better_candidate(a: &Candidate, b: &Candidate) -> bool {
    if a.orphan != b.orphan {
        return !a.orphan;
    }
    if (a.cell_w - b.cell_w).abs() > 0.01 {
        return a.cell_w > b.cell_w;
    }
    if (a.grid_w - b.grid_w).abs() > 0.01 {
        return a.grid_w > b.grid_w;
    }
    if (a.grid_h - b.grid_h).abs() > 0.01 {
        return a.grid_h > b.grid_h;
    }
    a.rows * a.cols < b.rows * b.cols
}

impl Layout {
    fn compute(
        screen_w: f32,
        screen_h: f32,
        n: usize,
        selected: usize,
        max_previews: usize,
    ) -> Self {
        if n == 0 {
            return Self {
                cell_w: 0.0,
                preview_w: 0.0,
                preview_h: 0.0,
                card_h: 0.0,
                rows: 0,
                cols: 0,
                visible: 0..0,
                grid_w: 0.0,
                grid_h: 0.0,
                strip_h: 0.0,
                panel_w: 0.0,
                panel_h: 0.0,
            };
        }

        let full = FullLayout::compute(screen_w, screen_h);
        let limit = max_previews.clamp(1, MAX_PREVIEWS).min(full.capacity());
        let visible_count = n.min(limit);
        let selected = selected.min(n.saturating_sub(1));

        let mut best: Option<Candidate> = None;
        for cols in 1..=full.cols {
            let rows = visible_count.div_ceil(cols);
            if rows > full.rows {
                continue;
            }

            let cell_by_width =
                (full.grid_w - (cols.saturating_sub(1)) as f32 * CARD_GAP) / cols as f32;
            let card_h_available =
                (full.grid_h - (rows.saturating_sub(1)) as f32 * ROW_GAP) / rows as f32;
            let cell_by_height = ((card_h_available - 2.0 * (CARD_PAD + CARD_BORDER)).max(1.0)
                / PREVIEW_ASPECT)
                + 2.0 * (CARD_PAD + CARD_BORDER);
            let cell_w = cell_by_width.min(cell_by_height).min(MAX_CARD_W);
            if cell_w < MIN_CARD_W {
                continue;
            }

            let candidate = Candidate::new(cols, rows, cell_w, visible_count);
            if best
                .as_ref()
                .is_none_or(|current| better_candidate(&candidate, current))
            {
                best = Some(candidate);
            }
        }

        // A single row of the full grid is always a safe fallback.
        let chosen = best.unwrap_or_else(|| {
            let cols = visible_count.min(full.cols).max(1);
            let rows = visible_count.div_ceil(cols).min(full.rows).max(1);
            Candidate::new(cols, rows, full.cell_w.min(MAX_CARD_W), visible_count)
        });

        let start = if n <= visible_count {
            0
        } else {
            selected
                .saturating_sub(visible_count / 2)
                .min(n - visible_count)
        };
        let visible = start..(start + visible_count).min(n);
        let strip_h = chosen.grid_h + COUNTER_BAR + LABEL_BAND;

        Self {
            cell_w: chosen.cell_w,
            preview_w: chosen.preview_w,
            preview_h: chosen.preview_h,
            card_h: chosen.card_h,
            rows: chosen.rows,
            cols: chosen.cols,
            visible,
            grid_w: chosen.grid_w,
            grid_h: chosen.grid_h,
            strip_h,
            panel_w: full.grid_w,
            panel_h: full.grid_h + COUNTER_BAR + LABEL_BAND,
        }
    }

    /// Top-left corner of a card inside the grid, in grid coordinates.  Rows
    /// with fewer cards are centered horizontally.
    fn card_origin(&self, index: usize) -> Option<(f32, f32)> {
        if !self.visible.contains(&index) || self.cols == 0 {
            return None;
        }
        let local = index - self.visible.start;
        let row = local / self.cols;
        let col = local % self.cols;
        let row_start = row * self.cols;
        let cards_in_row = (self.visible.len() - row_start).min(self.cols);
        let row_w =
            cards_in_row as f32 * self.cell_w + (cards_in_row.saturating_sub(1)) as f32 * CARD_GAP;
        let row_x = (self.grid_w - row_w) / 2.0;
        Some((
            row_x + col as f32 * (self.cell_w + CARD_GAP),
            row as f32 * (self.card_h + ROW_GAP),
        ))
    }
}

fn color(rgba: [f32; 4]) -> Color {
    Color::from_rgba(rgba[0], rgba[1], rgba[2], rgba[3])
}

/// Shortens `title` to what fits in `width` at LABEL_SIZE.  Character width is
/// only estimated, so this errs narrow.
#[cfg(test)]
fn fit_title(title: &str, width: f32) -> String {
    fit_title_lines(title, width, 1)
}

/// Shortens `title` to at most `max_lines` estimated lines at LABEL_SIZE.
#[cfg(test)]
fn fit_title_lines(title: &str, width: f32, max_lines: usize) -> String {
    let per_line = (width / (LABEL_SIZE as f32 * 0.55)).max(3.0) as usize;
    let max_chars = per_line.saturating_mul(max_lines.max(1)).max(3);
    if title.chars().count() <= max_chars {
        return title.to_string();
    }
    let kept: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// Choose one, two, or three lines for the selected-window label and return
/// its text and the height it needs.
fn label_text(title: &str, width: f32) -> (String, f32) {
    let per_line = (width / (LABEL_SIZE as f32 * 0.55)).max(3.0) as usize;
    let chars = title.chars().count();
    let lines = chars.div_ceil(per_line).clamp(1, 3);
    let max_chars = per_line * lines;
    let text = if chars <= max_chars {
        title.to_string()
    } else {
        let kept: String = title.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", kept.trim_end())
    };
    let height = match lines {
        1 => LABEL_LINE_H,
        2 => LABEL_TEXT_TWO_H,
        _ => LABEL_TEXT_THREE_H,
    };
    (text, height)
}

fn app_icon<'a>(visual: &AppVisual, size: u16) -> Element<'a, Message> {
    icon::icon(visual.handle.clone()).size(size).into()
}

/// Pixel size of the panel-sized layer surface.  A layer surface has to name
/// its size up front and clips anything past it, so the panel geometry is
/// worked out here once, for both the surface and the view.
pub fn panel_size(screen_w: f32, screen_h: f32) -> (u32, u32) {
    let full = FullLayout::compute(screen_w, screen_h);
    let width = (full.grid_w + 2.0 * STRIP_PAD + 2.0 * SURFACE_MARGIN).ceil() as u32;
    let height = (full.grid_h + COUNTER_BAR + LABEL_BAND + 2.0 * STRIP_PAD + 2.0 * SURFACE_MARGIN)
        .ceil() as u32;
    (width.max(1), height.max(1))
}

pub fn view(state: &AppSwitcher) -> Element<'_, Message> {
    responsive(move |size| {
        // The layer surface starts at a bogus 1x1 logical size until the
        // compositor configures it.  Keep that layout transparent; the real
        // configure is laid out immediately at the surface's actual size.
        if !state.visible
            || !size.width.is_finite()
            || !size.height.is_finite()
            || size.width <= 1.0
            || size.height <= 1.0
        {
            if state.visible {
                log::debug!(
                    "view waiting for configure: {}x{} after {:?}",
                    size.width,
                    size.height,
                    state.show_started.elapsed()
                );
            }
            return container(space())
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }

        // The surface is panel-sized now, so `size` is only used to confirm
        // that the compositor has configured it.  Lay the panel out from the
        // full output size so the 6x3 footprint and card sizes do not depend
        // on how much margin the surface happens to include.
        log::debug!(
            "view grid: surface {}x{}, screen {}x{} after {:?}",
            size.width,
            size.height,
            state.screen_size.0,
            state.screen_size.1,
            state.show_started.elapsed()
        );
        let layout_started = std::time::Instant::now();
        let layout = Layout::compute(
            state.screen_size.0,
            state.screen_size.1,
            state.toplevels.len(),
            state.selected,
            state.preview_count,
        );
        let layout_time = layout_started.elapsed();
        let strip_started = std::time::Instant::now();
        let strip = build_strip(state, &layout);
        log::debug!(
            "view built: layout {:?}, strip {:?}, total {:?}",
            layout_time,
            strip_started.elapsed(),
            state.show_started.elapsed()
        );

        // The surface is only as large as the strip plus shadow room, so the
        // desktop is not covered and the compositor only has a panel-sized
        // buffer to draw and present.
        container(strip)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    })
    .into()
}

fn build_card<'a>(state: &'a AppSwitcher, layout: &Layout, index: usize) -> Element<'a, Message> {
    let tv = &state.theme;
    let sel_bg = color(tv.selected_bg);
    let fg = color(tv.fg);
    let corner = tv.corner_radius;
    let selected = index == state.selected;
    let visual = state.toplevels.get(index).map(|entry| state.visual(entry));
    let preview = state
        .toplevels
        .get(index)
        .and_then(|entry| state.previews.get(&entry.identifier));

    let preview_el: Element<Message> = match preview {
        Some(handle) => image(handle.clone())
            .content_fit(ContentFit::Contain)
            .width(Length::Fixed(layout.preview_w))
            .height(Length::Fixed(layout.preview_h))
            .border_radius((corner - 4.0).max(2.0))
            .opacity(if selected { 1.0 } else { 0.62_f32 })
            .into(),
        None => {
            let icon_size = (layout.preview_w * 0.35).clamp(40.0, 128.0) as u16;
            let fallback = visual
                .as_ref()
                .map(|visual| app_icon(visual, icon_size))
                .unwrap_or_else(|| space().width(Length::Fixed(f32::from(icon_size))).into());
            container(fallback)
                .width(Length::Fixed(layout.preview_w))
                .height(Length::Fixed(layout.preview_h))
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .into()
        }
    };

    let card = container(preview_el)
        .width(Length::Fixed(layout.cell_w))
        .height(Length::Fixed(layout.card_h))
        .padding(CARD_PAD)
        .style(move |_: &cosmic::Theme| ContainerStyle {
            background: selected.then_some(Background::Color(sel_bg)),
            border: Border {
                radius: corner.into(),
                width: CARD_BORDER,
                color: if selected {
                    Color { a: 0.9, ..fg }
                } else {
                    Color { a: 0.12, ..fg }
                },
            },
            shadow: if selected {
                Shadow {
                    color: Color { a: 0.22, ..fg },
                    offset: Vector::new(0.0, 0.0),
                    blur_radius: 12.0,
                }
            } else {
                Shadow::default()
            },
            ..Default::default()
        });

    mouse_area(card)
        .on_press(Message::SelectIndex(index))
        .on_release(Message::Activate)
        .into()
}

/// The selected window's icon and title, sized to fit exactly under its
/// preview.  Long titles use at most two lines, and the pill is opaque enough
/// to stay readable when it overlaps the next row.
fn selected_label<'a>(state: &'a AppSwitcher, width: f32) -> Option<Element<'a, Message>> {
    let entry = state.toplevels.get(state.selected)?;
    let visual = state.visual(entry);
    let title = window_label(&entry.app_id, &entry.title, &visual);

    let text_width = (width - 2.0 * LABEL_PAD - f32::from(LABEL_ICON) - LABEL_GAP).max(24.0);
    let (label_text, text_height) = label_text(title, text_width);
    let label_height = text_height + 2.0 * LABEL_PAD_Y;

    let bg = color(state.theme.bg);
    let fg = color(state.theme.fg);
    let corner = state.theme.corner_radius;

    let text_el = text(label_text)
        .size(LABEL_SIZE)
        .width(Length::Fixed(text_width))
        .height(Length::Fixed(text_height))
        .wrapping(Wrapping::Word)
        .class(cosmic::theme::Text::Color(fg));

    let content = row![app_icon(&visual, LABEL_ICON), text_el]
        .spacing(LABEL_GAP)
        .align_y(Alignment::Center);

    Some(
        container(content)
            .width(Length::Fixed(width))
            .height(Length::Fixed(label_height))
            .padding([LABEL_PAD_Y, LABEL_PAD])
            .align_y(Alignment::Center)
            .style(move |_: &cosmic::Theme| ContainerStyle {
                background: Some(Background::Color(Color { a: 0.94, ..bg })),
                border: Border {
                    radius: (corner - 4.0).max(6.0).into(),
                    width: 1.0,
                    color: Color { a: 0.12, ..fg },
                },
                shadow: Shadow {
                    color: Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.5,
                    },
                    offset: Vector::new(0.0, 2.0),
                    blur_radius: 8.0,
                },
                ..Default::default()
            })
            .into(),
    )
}

fn counter_row<'a>(state: &'a AppSwitcher, layout: &Layout) -> Element<'a, Message> {
    let fg = color(state.theme.fg);
    let counter: Element<Message> = if state.toplevels.len() > layout.visible.len() {
        text(format!(
            "{} / {}",
            state.selected + 1,
            state.toplevels.len()
        ))
        .size(COUNTER_SIZE)
        .class(cosmic::theme::Text::Color(Color { a: 0.7, ..fg }))
        .into()
    } else {
        space().into()
    };

    row![space().width(Length::Fill), counter]
        .width(Length::Fixed(layout.panel_w))
        .height(Length::Fixed(COUNTER_BAR))
        .align_y(Alignment::Center)
        .into()
}

fn preview_grid<'a>(state: &'a AppSwitcher, layout: &Layout) -> Element<'a, Message> {
    let visible_len = layout.visible.len();
    debug_assert_eq!(
        layout.rows,
        visible_len.div_ceil(layout.cols.max(1)),
        "grid row count should match the visible cards"
    );
    let mut grid_rows = Vec::new();
    for row_start in (0..visible_len).step_by(layout.cols.max(1)) {
        let row_end = (row_start + layout.cols).min(visible_len);
        let cards = (layout.visible.start + row_start..layout.visible.start + row_end)
            .map(|index| build_card(state, layout, index))
            .collect::<Vec<_>>();
        grid_rows.push(row(cards).spacing(CARD_GAP).align_y(Alignment::Center));
    }

    column(grid_rows.into_iter().map(Into::into))
        .spacing(ROW_GAP)
        .align_x(Alignment::Center)
        .width(Length::Fixed(layout.grid_w))
        .height(Length::Fixed(layout.grid_h))
        .into()
}

fn selected_label_overlay<'a>(
    state: &'a AppSwitcher,
    layout: &Layout,
) -> Option<Element<'a, Message>> {
    let (card_x, card_y) = layout.card_origin(state.selected)?;
    let label = selected_label(state, layout.preview_w)?;
    let label_x =
        (layout.panel_w - layout.grid_w) / 2.0 + card_x + (layout.cell_w - layout.preview_w) / 2.0;
    let label_y = COUNTER_BAR + card_y + layout.card_h + LABEL_GAP_Y;

    Some(
        column![
            space().width(Length::Fill).height(Length::Fixed(label_y)),
            row![space().width(Length::Fixed(label_x)), label].width(Length::Fill),
        ]
        .width(Length::Fixed(layout.panel_w))
        .height(Length::Fixed(layout.strip_h))
        .into(),
    )
}

fn build_strip<'a>(state: &'a AppSwitcher, layout: &Layout) -> Element<'a, Message> {
    let tv = &state.theme;
    let bg = color(tv.bg);
    let corner = tv.corner_radius;

    let base = column![
        counter_row(state, layout),
        preview_grid(state, layout),
        space()
            .width(Length::Fill)
            .height(Length::Fixed(LABEL_BAND)),
    ]
    .spacing(0.0)
    .align_x(Alignment::Center)
    .width(Length::Fixed(layout.panel_w))
    .height(Length::Fixed(layout.strip_h));

    let content: Element<Message> = match selected_label_overlay(state, layout) {
        Some(overlay) => cosmic::iced::widget::stack(vec![base.into(), overlay])
            .width(Length::Fixed(layout.panel_w))
            .height(Length::Fixed(layout.strip_h))
            .into(),
        None => base.into(),
    };

    container(content)
        .style(move |_: &cosmic::Theme| ContainerStyle {
            background: Some(Background::Color(bg)),
            border: Border {
                radius: corner.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            shadow: Shadow {
                color: Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.6,
                },
                offset: Vector::new(0.0, 8.0),
                blur_radius: 32.0,
            },
            ..Default::default()
        })
        .width(Length::Fixed(layout.panel_w + 2.0 * STRIP_PAD))
        .height(Length::Fixed(layout.panel_h + 2.0 * STRIP_PAD))
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .padding(STRIP_PAD)
        .into()
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_GAP, COUNTER_BAR, LABEL_BAND, LABEL_LINE_H, LABEL_TEXT_THREE_H, LABEL_TEXT_TWO_H,
        Layout, MAX_CARD_W, MAX_COLS, MAX_PREVIEWS, MAX_ROWS, MIN_CARD_W, ROW_GAP, SCREEN_MARGIN,
        STRIP_PAD, SURFACE_MARGIN, fit_title, fit_title_lines, label_text, panel_size,
    };

    #[test]
    fn small_counts_use_one_row() {
        let layout = Layout::compute(1920.0, 1080.0, 3, 1, MAX_PREVIEWS);
        assert_eq!(layout.rows, 1);
        assert_eq!(layout.cols, 3);
        assert_eq!(layout.visible, 0..3);
        assert!(layout.cell_w <= MAX_CARD_W);
        assert!(layout.cell_w >= MIN_CARD_W);
    }

    #[test]
    fn seven_windows_use_two_balanced_rows() {
        let layout = Layout::compute(1920.0, 1080.0, 7, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cols, 4);
        assert_eq!(layout.visible, 0..7);
    }

    #[test]
    fn thirteen_windows_use_three_rows() {
        let layout = Layout::compute(1920.0, 1080.0, 13, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cols, 5);
        assert_eq!(layout.visible, 0..13);
    }

    #[test]
    fn eighteen_windows_fit_in_three_rows() {
        let layout = Layout::compute(1920.0, 1080.0, 18, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cols, 6);
        assert_eq!(layout.visible, 0..18);
        assert_eq!(layout.visible.len(), 18);
    }

    #[test]
    fn overflow_slides_and_keeps_selection_visible() {
        let layout = Layout::compute(1920.0, 1080.0, 25, 12, MAX_PREVIEWS);
        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cols, 6);
        assert_eq!(layout.visible.len(), 18);
        assert!(layout.visible.contains(&12));
        assert!(layout.visible.start > 0);

        let layout = Layout::compute(1920.0, 1080.0, 25, 24, MAX_PREVIEWS);
        assert!(layout.visible.contains(&24));
        assert_eq!(layout.visible.end, 25);
    }

    #[test]
    fn fractional_scale_surface_uses_six_columns() {
        // 1536x960 output at 1.25 scale configures the surface at 1228.8x768.
        let layout = Layout::compute(1228.8, 768.0, 19, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cols, 6);
        assert_eq!(layout.visible.len(), 18);
        assert!(layout.cell_w >= MIN_CARD_W);
        assert!(layout.strip_h + 2.0 * STRIP_PAD + 2.0 * SCREEN_MARGIN <= 768.0 + 1.0);
    }

    #[test]
    fn grid_fits_on_a_1366x768_screen() {
        let (screen_w, screen_h) = (1366.0, 768.0);
        let layout = Layout::compute(screen_w, screen_h, 18, 0, MAX_PREVIEWS);
        assert!(layout.rows <= MAX_ROWS);
        assert!(layout.cols <= MAX_COLS);
        assert!(layout.grid_w + 2.0 * STRIP_PAD + 2.0 * SCREEN_MARGIN <= screen_w + 1.0);
        assert!(layout.strip_h + 2.0 * STRIP_PAD + 2.0 * SCREEN_MARGIN <= screen_h + 1.0);
        assert!(layout.cell_w >= MIN_CARD_W);
    }

    #[test]
    fn short_screens_reduce_rows_instead_of_shrinking_cards() {
        let layout = Layout::compute(1920.0, 400.0, 18, 0, MAX_PREVIEWS);
        assert!(layout.rows < MAX_ROWS);
        assert!(layout.cell_w >= MIN_CARD_W);
        assert!(layout.strip_h + 2.0 * STRIP_PAD + 2.0 * SCREEN_MARGIN <= 400.0 + 1.0);
    }

    #[test]
    fn grid_width_includes_gaps() {
        let layout = Layout::compute(1920.0, 1080.0, 3, 0, MAX_PREVIEWS);
        let expected = layout.cols as f32 * layout.cell_w + (layout.cols - 1) as f32 * CARD_GAP;
        assert!((layout.grid_w - expected).abs() < 0.01);
    }

    #[test]
    fn grid_height_includes_row_gaps() {
        let layout = Layout::compute(1920.0, 1080.0, 7, 0, MAX_PREVIEWS);
        let expected = layout.rows as f32 * layout.card_h + (layout.rows - 1) as f32 * ROW_GAP;
        assert!((layout.grid_h - expected).abs() < 0.01);
        assert!((layout.strip_h - (layout.grid_h + COUNTER_BAR + LABEL_BAND)).abs() < 0.01);
    }

    #[test]
    fn preview_limit_six_uses_three_by_two() {
        let full = Layout::compute(1536.0, 960.0, 18, 0, MAX_PREVIEWS);
        let layout = Layout::compute(1536.0, 960.0, 6, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cols, 3);
        assert_eq!(layout.visible.len(), 6);
        assert!(layout.cell_w > full.cell_w);
        assert!((layout.panel_w - full.grid_w).abs() < 0.01);
        assert!((layout.panel_h - (full.grid_h + COUNTER_BAR + LABEL_BAND)).abs() < 0.01);
    }

    #[test]
    fn panel_reserves_counter_and_label_bands() {
        let layout = Layout::compute(1536.0, 960.0, 18, 0, MAX_PREVIEWS);
        assert!((layout.strip_h - (layout.grid_h + COUNTER_BAR + LABEL_BAND)).abs() < 0.01);
        assert!((layout.panel_h - (layout.grid_h + COUNTER_BAR + LABEL_BAND)).abs() < 0.01);
    }

    #[test]
    fn preview_limit_nine_uses_five_by_two() {
        let full = Layout::compute(1536.0, 960.0, 18, 0, MAX_PREVIEWS);
        let layout = Layout::compute(1536.0, 960.0, 9, 0, MAX_PREVIEWS);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cols, 5);
        assert_eq!(layout.visible.len(), 9);
        assert!(layout.cell_w > full.cell_w);
    }

    #[test]
    fn preview_page_slides_through_all_windows() {
        let layout = Layout::compute(1536.0, 960.0, 25, 24, 6);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cols, 3);
        assert_eq!(layout.visible.len(), 6);
        assert!(layout.visible.contains(&24));
        assert!(layout.visible.start > 0);
        assert_eq!(layout.visible.end, 25);
    }

    #[test]
    fn preview_limit_avoids_a_single_orphan() {
        let layout = Layout::compute(1536.0, 960.0, 13, 0, 13);
        assert_eq!(layout.rows, 3);
        assert_eq!(layout.cols, 5);
        assert_eq!(layout.visible.len(), 13);
    }

    #[test]
    fn preview_limit_bounds_are_clamped() {
        let one = Layout::compute(1536.0, 960.0, 18, 0, 0);
        assert_eq!(one.visible.len(), 1);
        let all = Layout::compute(1536.0, 960.0, 18, 0, 100);
        assert_eq!(all.visible.len(), MAX_PREVIEWS);
    }

    #[test]
    fn surface_is_panel_sized_with_shadow_margin() {
        let layout = Layout::compute(1536.0, 960.0, 6, 0, 6);
        let (width, height) = panel_size(1536.0, 960.0);
        assert_eq!(
            width,
            (layout.panel_w + 2.0 * STRIP_PAD + 2.0 * SURFACE_MARGIN).ceil() as u32
        );
        assert_eq!(
            height,
            (layout.panel_h + 2.0 * STRIP_PAD + 2.0 * SURFACE_MARGIN).ceil() as u32
        );
        assert!(width <= 1536, "{width}");
        assert!(height <= 960, "{height}");
        // Panel-sized, not full-output: a full-output surface would be 1536x960.
        assert!(u64::from(width) * u64::from(height) < 1536 * 960);
    }

    #[test]
    fn selected_card_position_centers_short_rows() {
        let layout = Layout::compute(1536.0, 960.0, 7, 6, MAX_PREVIEWS);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.cols, 4);
        let row_w = 3.0 * layout.cell_w + 2.0 * CARD_GAP;
        let row_x = (layout.grid_w - row_w) / 2.0;
        let (x, y) = layout.card_origin(4).unwrap();
        assert!((x - row_x).abs() < 0.01);
        assert!((y - (layout.card_h + ROW_GAP)).abs() < 0.01);
        let (x, _) = layout.card_origin(6).unwrap();
        assert!((x - (row_x + 2.0 * (layout.cell_w + CARD_GAP))).abs() < 0.01);
    }

    #[test]
    fn full_rows_start_at_grid_origin() {
        let layout = Layout::compute(1536.0, 960.0, 18, 0, MAX_PREVIEWS);
        assert_eq!(layout.card_origin(0), Some((0.0, 0.0)));
        let (x, y) = layout.card_origin(17).unwrap();
        assert!((x - 5.0 * (layout.cell_w + CARD_GAP)).abs() < 0.01);
        assert!((y - 2.0 * (layout.card_h + ROW_GAP)).abs() < 0.01);
    }

    #[test]
    fn label_text_selects_one_to_three_lines() {
        let (short, height) = label_text("Short", 200.0);
        assert_eq!(short, "Short");
        assert_eq!(height, LABEL_LINE_H);

        let (_, height) = label_text(&"x".repeat(20), 100.0);
        assert_eq!(height, LABEL_TEXT_TWO_H);

        let (text, height) = label_text(&"x".repeat(500), 100.0);
        assert_eq!(height, LABEL_TEXT_THREE_H);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn multiple_lines_fit_more_text() {
        let title = "A window title that is quite long but should fit on several lines";
        let one = fit_title(title, 100.0);
        let two = fit_title_lines(title, 100.0, 2);
        let three = fit_title_lines(title, 100.0, 3);
        assert!(one.ends_with('…'));
        assert!(two.chars().count() >= one.chars().count());
        assert!(three.chars().count() >= two.chars().count());
    }

    #[test]
    fn long_titles_are_shortened() {
        let fitted = fit_title(
            "A window title far too long to fit under the preview",
            160.0,
        );
        assert!(fitted.ends_with('…'), "{fitted:?}");
        assert_eq!(fit_title("Short", 160.0), "Short");
    }
}
