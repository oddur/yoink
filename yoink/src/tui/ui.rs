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

/// Carve the global breadcrumb header off the top of `area`. Returns
/// (`breadcrumb_area`, `pane_area`) so the App can render the
/// breadcrumb row and then dispatch the active pane into the rest.
#[must_use]
pub fn split_with_breadcrumb(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    (chunks[0], chunks[1])
}

/// Render a path of crumbs as `yoink › Hosts › backtrack-eu-1` —
/// the leaf crumb gets bold+cyan, the rest are dim. Right-aligns
/// `right` if non-empty (used for "5 hosts · refreshed 2s ago").
pub fn render_breadcrumb(frame: &mut Frame<'_>, area: Rect, crumbs: &[String], right: &str) {
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
