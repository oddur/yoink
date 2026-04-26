//! Service deploy history pane: lists past + current container
//! generations for a single service across every host, sorted
//! newest-first. Pressing `r` on a row triggers the reconcile-confirm
//! modal pre-populated with that row's tag — the same flow `yoink
//! rollback --tag <value>` exercises from the CLI.
//!
//! Data is the same shape `cmd_history` produces: every container
//! labeled `yoink.service=<name>` on every configured host. Stopped
//! containers from previous deploys are exactly the rollback targets,
//! and they live on the host until `yoink prune` removes them.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::docker_ops::ContainerInfo;
use crate::output::format_relative_time;

use super::ui::{TABLE_HIGHLIGHT_SYMBOL, bold, pane_layout, state_style, table_highlight_style};

/// One row in the history table — flattened from a host's container
/// list so the table can render without dipping back into nested
/// structures.
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub host: String,
    pub container: String,
    pub version: String,
    pub state: String,
    pub deployed_by: String,
    /// Sort key. `yoink.deployed-at` when present, else `created_unix`.
    /// `0` for ancient containers without either label — those sink
    /// to the bottom of the list.
    pub when_unix: i64,
}

impl HistoryRow {
    pub fn from_container(host: &str, c: &ContainerInfo) -> Self {
        Self {
            host: host.to_string(),
            container: c.name.clone(),
            version: c
                .yoink_version
                .clone()
                .unwrap_or_else(|| "?".to_string()),
            state: c.state.clone(),
            deployed_by: c
                .yoink_deployed_by
                .clone()
                .unwrap_or_else(|| "?".to_string()),
            when_unix: c.yoink_deployed_at.or(c.created_unix).unwrap_or(0),
        }
    }
}

#[derive(Default)]
pub struct HistoryState {
    /// `Some(name)` once the pane has been opened for a service. Used
    /// in render to title the pane even before `apply` lands.
    service: Option<String>,
    rows: Vec<HistoryRow>,
    table: TableState,
    loaded: bool,
}

impl HistoryState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the pane which service we're about to fetch history for.
    /// Called from `App::transition` *before* the async fetch resolves
    /// so the render shows "loading…" with the right title.
    pub fn set_service(&mut self, name: String) {
        if self.service.as_deref() != Some(name.as_str()) {
            self.rows.clear();
            self.table = TableState::default();
            self.loaded = false;
        }
        self.service = Some(name);
    }

    /// Apply a fresh fetch result. Sorts newest-first and pins the
    /// selection to the first row when the previous selection no
    /// longer fits.
    pub fn apply(&mut self, service: &str, mut rows: Vec<HistoryRow>) {
        if self.service.as_deref() != Some(service) {
            // Result raced a different transition — the user moved on.
            return;
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.when_unix));
        self.rows = rows;
        self.loaded = true;
        if self.rows.is_empty() {
            self.table.select(None);
        } else {
            let cur = self.table.selected().unwrap_or(0);
            self.table.select(Some(cur.min(self.rows.len() - 1)));
        }
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let next = self.table.selected().map_or(0, |i| (i + 1) % self.rows.len());
        self.table.select(Some(next));
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let prev = self
            .table
            .selected()
            .map_or(0, |i| if i == 0 { self.rows.len() - 1 } else { i - 1 });
        self.table.select(Some(prev));
    }

    /// Selected row's `(service, version)` pair — ready to feed into
    /// the reconcile-confirm modal as a `--tag` override.
    #[must_use]
    pub fn selected_rollback_target(&self) -> Option<(String, String)> {
        let svc = self.service.as_ref()?;
        let row = self.rows.get(self.table.selected()?)?;
        // Refuse "rollback to current" — pointless and surprising;
        // the operator can press `U` from Services to redeploy the
        // current spec instead.
        if row.state == "running" {
            return None;
        }
        Some((svc.clone(), row.version.clone()))
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let layout = pane_layout(area);
        let title = match &self.service {
            Some(svc) => format!("yoink history · {svc} · ↑↓ select · r rollback · esc back"),
            None => "yoink history".into(),
        };
        let header = Paragraph::new(title).style(bold());
        frame.render_widget(header, layout[0]);

        let widths = [
            Constraint::Length(8),  // when
            Constraint::Length(14), // version
            Constraint::Length(10), // state
            Constraint::Length(20), // host
            Constraint::Length(36), // container
            Constraint::Min(12),    // deployed-by
        ];
        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from("(loading…)")])]
        } else if self.rows.is_empty() {
            vec![Row::new(vec![Cell::from(
                "(no history — service has no yoink-managed containers on any host)",
            )])]
        } else {
            self.rows
                .iter()
                .map(|r| {
                    let when = if r.when_unix > 0 {
                        format_relative_time(Some(r.when_unix))
                    } else {
                        "?".into()
                    };
                    Row::new(vec![
                        Cell::from(when),
                        Cell::from(r.version.clone()),
                        Cell::from(r.state.clone()).style(state_style(&r.state)),
                        Cell::from(r.host.clone()),
                        Cell::from(r.container.clone()),
                        Cell::from(r.deployed_by.clone()),
                    ])
                })
                .collect()
        };
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("when").style(bold()),
                Cell::from("version").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("host").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("deployed-by").style(bold()),
            ]))
            .row_highlight_style(table_highlight_style())
            .highlight_symbol(TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("history"));
        frame.render_stateful_widget(table, layout[1], &mut self.table);

        let footer = Paragraph::new(
            "↑↓/jk navigate · r rollback to selected (running rows are skipped) · R refresh · esc back",
        );
        frame.render_widget(footer, layout[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn row(host: &str, version: &str, state: &str, when: i64) -> HistoryRow {
        HistoryRow {
            host: host.into(),
            container: format!("{version}-{host}"),
            version: version.into(),
            state: state.into(),
            deployed_by: "ci".into(),
            when_unix: when,
        }
    }

    #[test]
    fn apply_sorts_newest_first() {
        let mut s = HistoryState::new();
        s.set_service("api".into());
        s.apply(
            "api",
            vec![
                row("h", "v1", "exited", 100),
                row("h", "v3", "running", 300),
                row("h", "v2", "exited", 200),
            ],
        );
        let versions: Vec<&str> = s.rows.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, vec!["v3", "v2", "v1"]);
    }

    #[test]
    fn apply_for_other_service_is_dropped() {
        let mut s = HistoryState::new();
        s.set_service("api".into());
        s.apply("web", vec![row("h", "v1", "running", 100)]);
        assert!(s.rows.is_empty());
        assert!(!s.loaded);
    }

    #[test]
    fn selected_rollback_target_skips_running_row() {
        let mut s = HistoryState::new();
        s.set_service("api".into());
        s.apply(
            "api",
            vec![
                row("h", "v3", "running", 300),
                row("h", "v2", "exited", 200),
            ],
        );
        // Default selection is row 0 → running → no target.
        assert_eq!(s.selected_rollback_target(), None);
        s.select_next(); // → v2 exited
        assert_eq!(
            s.selected_rollback_target(),
            Some(("api".into(), "v2".into()))
        );
    }

    #[test]
    fn navigation_wraps() {
        let mut s = HistoryState::new();
        s.set_service("api".into());
        s.apply(
            "api",
            vec![
                row("h", "v3", "running", 300),
                row("h", "v2", "exited", 200),
            ],
        );
        s.select_next();
        s.select_next();
        assert_eq!(s.table.selected(), Some(0));
        s.select_prev();
        assert_eq!(s.table.selected(), Some(1));
    }
}
