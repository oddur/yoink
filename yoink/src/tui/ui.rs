//! Style + layout helpers shared by all panes.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::TableState;

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
