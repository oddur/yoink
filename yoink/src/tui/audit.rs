//! Audit-log pane. Renders the same merged operator + per-host JSONL
//! stream that `yoink audit log` produces, with a stable selection
//! anchored on `event_id`, an `Enter`-to-expand detail view, and a `/`
//! substring filter.
//!
//! Data flow mirrors `tui::history`: the pane owns rendered rows; the
//! `App` schedules an async `audit::fetch_events` and posts the result
//! back through the update bus.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::audit::{AuditEvent, AuditEventKind, event_name, event_summary};

use super::ui::{TABLE_HIGHLIGHT_SYMBOL, bold, pane_layout, table_highlight_style};

/// One pre-formatted row in the audit table. Holds the raw event so
/// the detail-expand pane can render the full payload without re-
/// querying the source.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub event_id: String,
    pub ts: String,
    pub origin: String,
    pub host: String,
    pub deploy_id: String,
    pub kind_name: &'static str,
    pub summary: String,
    pub raw: AuditEvent,
}

impl From<AuditEvent> for AuditRow {
    fn from(ev: AuditEvent) -> Self {
        Self {
            event_id: ev.event_id.clone(),
            ts: ev.ts.clone(),
            origin: ev.origin.clone(),
            host: ev.host.clone(),
            deploy_id: ev.deploy_id.clone(),
            kind_name: event_name(&ev.kind),
            summary: event_summary(&ev.kind),
            raw: ev,
        }
    }
}

#[derive(Default)]
pub struct AuditState {
    rows: Vec<AuditRow>,
    table: TableState,
    loaded: bool,
    /// `(source, message)` from the last fetch. Surfaced in the footer
    /// so the operator can tell "fleet is quiet" from "we couldn't
    /// reach a host".
    errors: Vec<(String, String)>,
    /// Current `/`-substring filter applied to (host, kind, summary,
    /// `deploy_id`, actor) for live narrowing. `None` means show all.
    filter: Option<String>,
    /// True while the operator is typing into the filter input. The
    /// footer renders the prompt and key dispatch routes characters
    /// into `pending_filter`.
    editing_filter: bool,
    pending_filter: String,
    /// `Enter` toggles the detail panel below the table.
    expanded: bool,
}

impl AuditState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fresh fetch result (already deduped + sorted newest-
    /// first by `audit::fetch_events`). Pins the selection to the
    /// previously-selected `event_id` when it survives the refresh.
    pub fn apply(&mut self, events: Vec<AuditEvent>, errors: Vec<(String, String)>) {
        let prev_id = self.selected_event_id().map(str::to_string);
        self.rows = events.into_iter().map(AuditRow::from).collect();
        self.errors = errors;
        self.loaded = true;
        let visible_len = self.visible_rows().count();
        if visible_len == 0 {
            self.table.select(None);
            return;
        }
        let new_idx = prev_id
            .and_then(|id| {
                self.visible_rows()
                    .position(|(_, r)| r.event_id == id)
            })
            .unwrap_or(0);
        self.table.select(Some(new_idx.min(visible_len - 1)));
    }

    /// Iterator over `(absolute_index, row)` for rows that pass the
    /// current substring filter. Used by both render and selection
    /// math so the indexed positions agree.
    fn visible_rows(&self) -> impl Iterator<Item = (usize, &AuditRow)> {
        let needle = self.filter.as_deref().map(str::to_lowercase);
        self.rows.iter().enumerate().filter(move |(_, r)| {
            let Some(needle) = needle.as_deref() else {
                return true;
            };
            row_matches(r, needle)
        })
    }

    pub fn select_next(&mut self) {
        let len = self.visible_rows().count();
        if len == 0 {
            return;
        }
        let next = self.table.selected().map_or(0, |i| (i + 1) % len);
        self.table.select(Some(next));
    }

    pub fn select_prev(&mut self) {
        let len = self.visible_rows().count();
        if len == 0 {
            return;
        }
        let prev = self
            .table
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.table.select(Some(prev));
    }

    pub fn toggle_expand(&mut self) {
        self.expanded = !self.expanded;
    }

    pub fn begin_filter(&mut self) {
        self.editing_filter = true;
        self.pending_filter = self.filter.clone().unwrap_or_default();
    }

    pub fn type_filter(&mut self, c: char) {
        if self.editing_filter {
            self.pending_filter.push(c);
        }
    }

    pub fn backspace_filter(&mut self) {
        if self.editing_filter {
            self.pending_filter.pop();
        }
    }

    /// Commit the in-flight filter. Empty input clears the filter.
    pub fn commit_filter(&mut self) {
        self.editing_filter = false;
        self.filter = if self.pending_filter.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending_filter))
        };
        self.pending_filter.clear();
        // Selection may have moved out of the new visible range.
        let len = self.visible_rows().count();
        match self.table.selected() {
            Some(i) if i >= len && len > 0 => self.table.select(Some(len - 1)),
            Some(_) if len == 0 => self.table.select(None),
            None if len > 0 => self.table.select(Some(0)),
            _ => {}
        }
    }

    pub fn cancel_filter(&mut self) {
        self.editing_filter = false;
        self.pending_filter.clear();
    }

    pub fn clear_filter(&mut self) {
        self.filter = None;
        self.editing_filter = false;
        self.pending_filter.clear();
    }

    #[must_use]
    pub fn editing_filter(&self) -> bool {
        self.editing_filter
    }

    /// True when a substring filter is set (Esc semantics: first press
    /// clears the filter, second press leaves the pane).
    #[must_use]
    pub fn filter_active(&self) -> bool {
        self.filter.is_some()
    }

    /// `event_id` of the currently selected row, if any visible row
    /// is selected.
    fn selected_event_id(&self) -> Option<&str> {
        let i = self.table.selected()?;
        self.visible_rows()
            .nth(i)
            .map(|(_, r)| r.event_id.as_str())
    }

    fn selected_row(&self) -> Option<&AuditRow> {
        let i = self.table.selected()?;
        self.visible_rows().nth(i).map(|(_, r)| r)
    }

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let layout = pane_layout(area);

        // Title bar shows totals + filter status; mirrors history.rs's
        // single-line header.
        let total = self.rows.len();
        let visible = self.visible_rows().count();
        let title_left = format!("yoink audit · {visible}/{total} events");
        let title_filter = match (&self.filter, self.editing_filter) {
            (_, true) => format!(" · filter (editing): {}", self.pending_filter),
            (Some(f), _) => format!(" · filter: {f}"),
            (None, _) => String::new(),
        };
        let title_errors = if self.errors.is_empty() {
            String::new()
        } else {
            format!(" · {} error(s)", self.errors.len())
        };
        let header = Paragraph::new(format!("{title_left}{title_filter}{title_errors}"))
            .style(bold());
        frame.render_widget(header, layout[0]);

        // Split body: table on top, optional detail panel below when
        // expanded. When not expanded the detail panel collapses to 0.
        let body_layout = if self.expanded {
            Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).split(layout[1])
        } else {
            Layout::vertical([Constraint::Min(5), Constraint::Length(0)]).split(layout[1])
        };

        self.render_table(frame, body_layout[0], throbber);
        if self.expanded {
            self.render_detail(frame, body_layout[1]);
        }

        let footer = self.footer_line();
        frame.render_widget(footer, layout[2]);
    }

    fn render_table(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let widths = [
            Constraint::Length(24), // ts
            Constraint::Length(8),  // origin
            Constraint::Length(20), // host
            Constraint::Length(20), // event
            Constraint::Min(20),    // summary
        ];

        let visible: Vec<&AuditRow> = self.visible_rows().map(|(_, r)| r).collect();

        let body_rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(throbber))])]
        } else if self.rows.is_empty() && !self.errors.is_empty() {
            vec![Row::new(vec![Cell::from(
                "(no audit events fetched — every source failed; see footer)",
            )])]
        } else if self.rows.is_empty() {
            vec![Row::new(vec![Cell::from(
                "(no audit events — fleet has had no state-changing yoink runs)",
            )])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from(
                "(filter matches no rows — esc to clear)",
            )])]
        } else {
            visible
                .iter()
                .map(|r| {
                    Row::new(vec![
                        Cell::from(r.ts.clone()),
                        Cell::from(r.origin.clone()).style(origin_style(&r.origin)),
                        Cell::from(host_display(&r.host).to_string()),
                        Cell::from(r.kind_name).style(event_kind_style(&r.raw.kind)),
                        Cell::from(r.summary.clone()),
                    ])
                })
                .collect()
        };

        let table = Table::new(body_rows, widths)
            .header(Row::new(vec![
                Cell::from("ts").style(bold()),
                Cell::from("origin").style(bold()),
                Cell::from("host").style(bold()),
                Cell::from("event").style(bold()),
                Cell::from("summary").style(bold()),
            ]))
            .row_highlight_style(table_highlight_style())
            .highlight_symbol(TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("audit"));
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        use std::fmt::Write as _;
        let body = self.selected_row().map_or_else(
            || "(no row selected)".to_string(),
            |row| {
                let mut out = String::new();
                let actor_line = match &row.raw.git_sha {
                    Some(sha) => format!(
                        "{} · yoink {} · git_sha:{}",
                        row.raw.actor, row.raw.yoink_version, sha
                    ),
                    None => format!("{} · yoink {}", row.raw.actor, row.raw.yoink_version),
                };
                let _ = writeln!(out, "event_id  : {}", row.event_id);
                let _ = writeln!(out, "deploy_id : {}", row.deploy_id);
                let _ = writeln!(out, "actor     : {actor_line}");
                let _ = writeln!(out, "summary   : {}", row.summary);
                if let AuditEventKind::DeployFailed { log_tail, .. } = &row.raw.kind {
                    let _ = writeln!(out, "log_tail  : {} line(s)", log_tail.len());
                    for line in log_tail.iter().take(6) {
                        let _ = writeln!(out, "  {}", line.trim_end());
                    }
                    if log_tail.len() > 6 {
                        let _ = writeln!(out, "  … ({} more)", log_tail.len() - 6);
                    }
                }
                out
            },
        );
        let detail = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("detail"));
        frame.render_widget(detail, area);
    }

    fn footer_line(&self) -> Paragraph<'_> {
        let (text, style) = if self.editing_filter {
            (
                format!(
                    "/ {} · enter commit · esc cancel",
                    self.pending_filter
                ),
                Style::default().fg(Color::Cyan),
            )
        } else if !self.errors.is_empty() {
            let detail = self
                .errors
                .iter()
                .map(|(s, m)| format!("{s}: {m}"))
                .collect::<Vec<_>>()
                .join(" · ");
            (
                format!("⚠ {} source(s) failed: {detail}", self.errors.len()),
                Style::default().fg(Color::Yellow),
            )
        } else {
            (
                "↑↓/jk select · enter expand · / filter · r refresh · esc back".to_string(),
                Style::default(),
            )
        };
        Paragraph::new(text).style(style)
    }
}

fn row_matches(r: &AuditRow, needle: &str) -> bool {
    r.host.to_lowercase().contains(needle)
        || r.kind_name.to_lowercase().contains(needle)
        || r.summary.to_lowercase().contains(needle)
        || r.deploy_id.to_lowercase().contains(needle)
        || r.raw.actor.to_lowercase().contains(needle)
}

fn host_display(host: &str) -> &str {
    if host.is_empty() { "—" } else { host }
}

fn origin_style(origin: &str) -> Style {
    if origin == "operator" {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::Cyan)
    }
}

fn event_kind_style(kind: &AuditEventKind) -> Style {
    use AuditEventKind as K;
    match kind {
        K::DeployFailed { .. } | K::RunFinished { ok: false, .. } => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        K::ContainerCreated { .. } | K::RunFinished { ok: true, .. } => {
            Style::default().fg(Color::Green)
        }
        K::RollbackStarted { .. } | K::RollbackFinished { .. } => Style::default().fg(Color::Yellow),
        _ => Style::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEventKind, build_event, RunContext};
    use pretty_assertions::assert_eq;

    fn ctx() -> RunContext {
        RunContext {
            deploy_id: "01HFE9TESTTESTTESTTESTTEST".into(),
            actor: "alice@laptop".into(),
            yoink_version: "0.18.0".into(),
            git_sha: Some("abc1234".into()),
            command: "up".into(),
        }
    }

    fn ev(host: &str, kind: AuditEventKind) -> AuditEvent {
        build_event(&ctx(), host, kind)
    }

    #[test]
    fn apply_loads_rows_and_keeps_first_selected() {
        let mut s = AuditState::new();
        s.apply(
            vec![
                ev("h1", AuditEventKind::LockAcquired),
                ev("h1", AuditEventKind::LockReleased),
            ],
            Vec::new(),
        );
        assert!(s.loaded);
        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.table.selected(), Some(0));
    }

    #[test]
    fn filter_narrows_visible_rows() {
        let mut s = AuditState::new();
        s.apply(
            vec![
                ev("h1", AuditEventKind::LockAcquired),
                ev(
                    "h2",
                    AuditEventKind::ContainerCreated {
                        service: "api".into(),
                        container: "api-ab12cd34".into(),
                        spec_hash: "ab12cd34".into(),
                        tag: "v1".into(),
                    },
                ),
            ],
            Vec::new(),
        );
        s.begin_filter();
        for c in "lock".chars() {
            s.type_filter(c);
        }
        s.commit_filter();
        let visible: Vec<&AuditRow> = s.visible_rows().map(|(_, r)| r).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].kind_name, "LockAcquired");
    }

    #[test]
    fn filter_empty_input_clears_filter() {
        let mut s = AuditState::new();
        s.apply(
            vec![ev("h1", AuditEventKind::LockAcquired)],
            Vec::new(),
        );
        s.begin_filter();
        s.commit_filter();
        assert!(s.filter.is_none());
    }

    #[test]
    fn navigation_wraps_within_visible() {
        let mut s = AuditState::new();
        s.apply(
            vec![
                ev("h1", AuditEventKind::LockAcquired),
                ev("h1", AuditEventKind::LockReleased),
            ],
            Vec::new(),
        );
        s.select_next();
        assert_eq!(s.table.selected(), Some(1));
        s.select_next();
        assert_eq!(s.table.selected(), Some(0));
        s.select_prev();
        assert_eq!(s.table.selected(), Some(1));
    }

    #[test]
    fn apply_preserves_selection_by_event_id_across_refresh() {
        let mut s = AuditState::new();
        let first = ev("h1", AuditEventKind::LockAcquired);
        let second = ev("h1", AuditEventKind::LockReleased);
        s.apply(vec![first.clone(), second.clone()], Vec::new());
        s.select_next();
        let pinned_id = s.selected_event_id().map(str::to_string);
        // Refresh injects a new (newer) event at the top; pinned row
        // should still be the same logical event.
        let third = ev("h1", AuditEventKind::HookStarted { name: "post".into() });
        s.apply(vec![third, first, second], Vec::new());
        assert_eq!(
            s.selected_event_id().map(str::to_string),
            pinned_id,
            "selection should follow the event_id across refreshes",
        );
    }

    #[test]
    fn errors_propagate_to_state() {
        let mut s = AuditState::new();
        s.apply(
            Vec::new(),
            vec![("nonexistent.invalid".into(), "DNS failed".into())],
        );
        assert!(s.rows.is_empty());
        assert_eq!(s.errors.len(), 1);
        assert!(s.loaded);
    }
}
