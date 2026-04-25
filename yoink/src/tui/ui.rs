//! Style + layout helpers shared by all panes.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, TableState};
use ratatui::Frame;

/// Bold style for table headers and pane titles.
#[must_use]
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Color for the "health" column based on docker's status text hint.
/// Returns the default style for unknown / no-healthcheck containers.
#[must_use]
pub fn health_style(health: &str) -> Style {
    match health {
        "healthy" => Style::default().fg(Color::Green),
        "unhealthy" => Style::default().fg(Color::Red),
        "starting" => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    }
}

/// Color for the "state" column based on docker's container state.
/// Lifecycle (running → exited / dead) maps onto the obvious colors;
/// transient states get yellow so they stand out at a glance.
#[must_use]
pub fn state_style(state: &str) -> Style {
    match state.to_ascii_lowercase().as_str() {
        "running" => Style::default().fg(Color::Green),
        "exited" | "dead" => Style::default().fg(Color::DarkGray),
        "restarting" | "removing" | "paused" => Style::default().fg(Color::Yellow),
        "created" => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}

/// Carve the global header off the top of `area`. The header is a
/// 4-row bordered block hosting the tab bar (row 1) + breadcrumb
/// (row 2). Returns (`header_area`, `pane_area`).
#[must_use]
pub fn split_with_header(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);
    (chunks[0], chunks[1])
}

/// Render the global header: a bordered cyan block containing a
/// `Tabs` row over a breadcrumb row. `tabs` is the ordered list of
/// top-level section labels; `selected_tab` is the currently active
/// index (or `None` to dim the whole bar — used inside drill-down
/// views that don't map cleanly to a single top-level tab).
#[allow(clippy::too_many_arguments)]
pub fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    tabs: &[&str],
    selected_tab: Option<usize>,
    crumbs: &[String],
    right: &str,
) {
    use ratatui::widgets::Tabs;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(Span::styled(
            " yoink ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let tab_titles: Vec<Line<'_>> = tabs.iter().map(|t| Line::from(*t)).collect();
    let tabs_widget = Tabs::new(tab_titles)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .divider(Span::styled(" │ ", Style::default().fg(Color::DarkGray)))
        .select(selected_tab.unwrap_or(usize::MAX));
    frame.render_widget(tabs_widget, inner_chunks[0]);

    render_breadcrumb_line(frame, inner_chunks[1], crumbs, right);
}

fn render_breadcrumb_line(frame: &mut Frame<'_>, area: Rect, crumbs: &[String], right: &str) {
    let dim = Style::default().fg(Color::DarkGray);
    let leaf = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let last = crumbs.len().saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(crumbs.len() * 2);
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" › ".to_string(), dim));
        }
        let style = if i == last { leaf } else { dim };
        spans.push(Span::styled(c.clone(), style));
    }
    let left = Paragraph::new(Line::from(spans));
    frame.render_widget(left, area);

    if !right.is_empty() {
        let right_width = u16::try_from(right.chars().count()).unwrap_or(u16::MAX);
        if area.width > right_width {
            let right_area = Rect {
                x: area.x + area.width - right_width,
                y: area.y,
                width: right_width,
                height: 1,
            };
            let right_para =
                Paragraph::new(Line::from(Span::styled(right.to_string(), dim)));
            frame.render_widget(right_para, right_area);
        }
    }
}

/// Style applied to the highlighted row in selectable tables
/// (`Hosts`, `HostDetail`, `Services`, `ServiceDetail`). Reverse-video
/// on cyan reads as "this is the cursor" without fighting any of the
/// per-cell colors (state/health/etc.) underneath.
#[must_use]
pub fn table_highlight_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// `▶ ` prefix on the highlighted row — pairs with
/// `table_highlight_style`. Used by every selectable table.
pub const TABLE_HIGHLIGHT_SYMBOL: &str = "▶ ";

/// Inline mini-gauge for a table cell, drawn with the same eighth-blocks
/// the real `Gauge` widget uses. Returns three styled spans: opening
/// bracket, the bar (colored fill + dim unfilled), closing bracket.
/// The brackets make the bounds obvious — without them the bar floats
/// in whitespace and you can't tell where 0% / 100% sit.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn inline_gauge(ratio: f32, width: usize, color: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let bracket_style = Style::default().fg(Color::DarkGray);
    let r = ratio.clamp(0.0, 1.0);
    let total_eighths = (r * (width as f32) * 8.0).round() as usize;
    let full = total_eighths / 8;
    let partial_idx = total_eighths % 8;
    let partials = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

    let mut filled = String::with_capacity(width);
    for _ in 0..full.min(width) {
        filled.push('█');
    }
    if full < width && partial_idx > 0 {
        filled.push(partials[partial_idx]);
    }
    // Pad the remaining slots with a faint dotted glyph so the
    // unfilled portion is visible — pure spaces look like missing
    // data on most terminals.
    let used = filled.chars().count();
    let mut empty = String::with_capacity(width.saturating_sub(used));
    for _ in 0..width.saturating_sub(used) {
        empty.push('·');
    }

    vec![
        Span::styled("[".to_string(), bracket_style),
        Span::styled(filled, Style::default().fg(color)),
        Span::styled(empty, bracket_style),
        Span::styled("]".to_string(), bracket_style),
    ]
}

/// Render a vertical scrollbar overlay on the right edge of `area`.
/// `position` is the index of the topmost visible item; `total` is
/// the number of items in the underlying buffer; `viewport` is how
/// many rows fit on screen. No-op if everything fits.
pub fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    position: usize,
    total: usize,
    viewport: usize,
) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    if total <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(total.saturating_sub(viewport)).position(position);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(bar, area, &mut state);
}

/// Render a centered modal overlay with `lines` of text, sized to fit
/// the longest line. Used for the `?` help overlay; could host other
/// modal dialogs later. Caller is responsible for matching key
/// handling (Esc / `?` to dismiss).
pub fn render_modal(frame: &mut Frame<'_>, title: &str, lines: &[&str]) {
    let area = frame.area();
    let max_width = lines
        .iter()
        .map(|l| u16::try_from(l.chars().count()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(40)
        .saturating_add(4);
    let modal_width = max_width.min(area.width.saturating_sub(4));
    let modal_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(modal_width)) / 2;
    let y = (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_width,
        height: modal_height,
    };
    let body = lines.iter().copied().map(Line::from).collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(Clear, modal_area);
    frame.render_widget(Paragraph::new(body).block(block), modal_area);
}

/// Standard pane layout: 1-row header / fill / 1-row footer.
#[must_use]
pub fn pane_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area)
}

/// Keep a `TableState` selection valid after the underlying row count
/// changes. Picks row 0 when there's no current selection but rows
/// exist; clamps to last row when the current selection is past the end.
pub fn clamp_selection(table: &mut TableState, len: usize) {
    if table.selected().is_none() && len > 0 {
        table.select(Some(0));
    } else if let Some(i) = table.selected()
        && i >= len
    {
        table.select(len.checked_sub(1));
    }
}
