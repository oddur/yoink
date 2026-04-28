//! TUI doctor pane — runs the same checks `yoink doctor` (CLI) does
//! and renders the findings as a modal overlay on top of the current
//! view. Press `D` from any view to open; `r` re-runs; Esc closes.
//!
//! Minimal state: a status enum + the most recently loaded findings.
//! The actual checks happen in `crate::doctor`; this module is purely
//! UI plumbing.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use throbber_widgets_tui::ThrobberState;

use crate::doctor::{Finding, Severity};

#[derive(Default)]
pub struct DoctorState {
    status: Status,
    /// Currently-selected row in the findings list. Persists across
    /// re-runs so a `r` refresh keeps the operator's place.
    selected: ListState,
}

#[derive(Default)]
enum Status {
    #[default]
    Closed,
    Loading,
    Loaded(Vec<Finding>),
}

impl DoctorState {
    #[must_use]
    pub fn is_open(&self) -> bool {
        !matches!(self.status, Status::Closed)
    }

    pub fn mark_loading(&mut self) {
        self.status = Status::Loading;
    }

    pub fn close(&mut self) {
        self.status = Status::Closed;
    }

    pub fn store(&mut self, findings: Vec<Finding>) {
        let len = findings.len();
        self.status = Status::Loaded(findings);
        // Park selection at the first row so common case ("read top
        // to bottom") needs no nav. Prior selection rarely makes
        // sense across runs because findings can reorder.
        self.selected.select(if len > 0 { Some(0) } else { None });
    }

    pub fn select_next(&mut self) {
        if let Status::Loaded(findings) = &self.status
            && !findings.is_empty()
        {
            let i = self.selected.selected().unwrap_or(0);
            self.selected.select(Some((i + 1).min(findings.len() - 1)));
        }
    }

    pub fn select_prev(&mut self) {
        if let Status::Loaded(_) = &self.status
            && let Some(i) = self.selected.selected()
            && i > 0
        {
            self.selected.select(Some(i - 1));
        }
    }
}

pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut DoctorState,
    throbber: &ThrobberState,
) {
    if !state.is_open() {
        return;
    }
    // Centred modal — 90% wide, 70% tall, capped to keep readability.
    let modal = centred_rect(area, 90, 70, 110, 30);
    frame.render_widget(Clear, modal);

    let outer = Block::default()
        .title(" doctor — diagnose deploy-blockers ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer.clone(), modal);
    let inner = outer.inner(modal);

    // Footer for keybinds — claim a single line at the bottom.
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    match &state.status {
        Status::Closed => {}
        Status::Loading => {
            let p = Paragraph::new(super::ui::throbber_with_label(throbber, " running checks…"));
            frame.render_widget(p, body);
        }
        Status::Loaded(findings) => {
            // Two columns: the scrollable list of findings, and a
            // detail pane on the right showing the selected finding's
            // detail + fix.
            let [list_area, detail_area] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body);

            let items: Vec<ListItem> = findings
                .iter()
                .map(|f| {
                    let (icon, color) = match f.severity {
                        Severity::Pass => ("✓", Color::Green),
                        Severity::Warn => ("!", Color::Yellow),
                        Severity::Error => ("✗", Color::Red),
                    };
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{icon} "),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("[{}] ", f.category),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(f.title.clone()),
                    ]);
                    ListItem::new(line)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED))
                .highlight_symbol(" ");
            frame.render_stateful_widget(list, list_area, &mut state.selected);

            let detail_text = state
                .selected
                .selected()
                .and_then(|i| findings.get(i))
                .map_or_else(|| vec![Line::from("(no selection)")], detail_lines);
            let detail = Paragraph::new(detail_text)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::default().borders(Borders::LEFT));
            frame.render_widget(detail, detail_area);
        }
    }

    let summary = match &state.status {
        Status::Loaded(findings) => {
            let (pass, warn, err) = crate::doctor::tally(findings);
            format!("{pass} pass, {warn} warn, {err} error  —  ↑↓ navigate, r rerun, Esc close")
        }
        _ => "r rerun, Esc close".to_string(),
    };
    let footer_p = Paragraph::new(summary).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer_p, footer);
}

fn detail_lines(f: &Finding) -> Vec<Line<'_>> {
    let mut out = vec![
        Line::from(Span::styled(
            f.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if let Some(d) = &f.detail {
        for line in d.lines() {
            out.push(Line::from(Span::raw(line.to_string())));
        }
        out.push(Line::from(""));
    }
    if let Some(fix) = &f.fix {
        out.push(Line::from(Span::styled(
            "fix:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for line in fix.lines() {
            out.push(Line::from(Span::raw(format!("  {line}"))));
        }
    }
    out
}

/// Centred rectangle inside `area`, expressed as a percentage of
/// width and height with hard maxes for readability on big terminals.
fn centred_rect(area: Rect, pct_w: u16, pct_h: u16, max_w: u16, max_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).min(max_w);
    let h = (area.height * pct_h / 100).min(max_h);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
