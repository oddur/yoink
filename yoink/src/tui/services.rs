//! Service-centric views — `Services` (one row per service in the
//! config, with running-count and `image:tag`) and `ServiceDetail`
//! (one row per running container of the selected service, across
//! every host). Drill path:
//!   `s` Services → Enter → `ServiceDetail` → Enter → `ContainerLogs`

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::config::Config;
use crate::docker_ops::{ContainerInfo, Host};
use crate::status::StatusReport;

use super::ui::{
    bold, clamp_selection, filter_footer, health_style, pane_layout, state_style, FilterState,
};

#[derive(Default)]
pub struct ServicesState {
    report: Option<StatusReport>,
    table: TableState,
    /// Service names in the order they appear in the config; the table
    /// uses this so navigation order matches what the operator wrote.
    service_names: Vec<String>,
    loaded: bool,
    pub filter: FilterState,
}

#[derive(Default)]
pub struct ServiceDetailState {
    service: Option<String>,
    table: TableState,
    /// Flattened (host, container) pairs in row order — keeps `Enter`
    /// drill-through aligned with what the user sees.
    rows: Vec<ServiceContainerRow>,
    loaded: bool,
    pub filter: FilterState,
}

#[derive(Debug, Clone)]
pub struct ServiceContainerRow {
    pub host: Host,
    pub container: ContainerInfo,
}

impl ServicesState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refresh data is shared with the dashboard — the caller hands us
    /// the latest `StatusReport` so we don't fetch a second time per
    /// tick. `service_names` reflects the config's declared order.
    pub fn apply(&mut self, report: Option<StatusReport>, service_names: Vec<String>) {
        if let Some(r) = report {
            self.report = Some(r);
            self.service_names = service_names;
            self.loaded = true;
            clamp_selection(&mut self.table, self.service_names.len());
        }
    }

    pub fn select_next(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_indices().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    pub fn selected_service(&self) -> Option<String> {
        let visible = self.visible_indices();
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .and_then(|src| self.service_names.get(src))
            .cloned()
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.service_names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| self.filter.matches(n).then_some(i))
            .collect()
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, config: &Config) {
        let layout = pane_layout(area);
        let header = Paragraph::new(format!(
            "yoink services · {} declared · ↑↓ select · enter for instances",
            config.services.len()
        ))
        .style(bold());
        frame.render_widget(header, layout[0]);

        let widths = [
            Constraint::Length(20), // service
            Constraint::Length(40), // image:tag
            Constraint::Length(12), // running
            Constraint::Length(10), // health
            Constraint::Min(20),    // hosts
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());
        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from("(loading…)")])]
        } else if visible.is_empty() && !self.service_names.is_empty() {
            vec![Row::new(vec![Cell::from("(no services match filter)")])]
        } else {
            visible
                .iter()
                .map(|i| build_service_row(&self.service_names[*i], config, self.report.as_ref()))
                .collect()
        };
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("service").style(bold()),
                Cell::from("image:tag").style(bold()),
                Cell::from("running").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("hosts").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("services"));
        frame.render_stateful_widget(table, layout[1], &mut self.table);

        let footer = filter_footer(
            &self.filter,
            "q quit · ↑↓ select · enter detail · r refresh · ? help",
        );
        frame.render_widget(footer, layout[2]);
    }
}

fn build_service_row<'a>(
    name: &'a str,
    config: &'a Config,
    report: Option<&StatusReport>,
) -> Row<'a> {
    let cfg_service = config.services.iter().find(|s| s.name == name);
    let image_tag = cfg_service.map_or_else(
        || "?".into(),
        |s| format!("{}:{}", s.image, s.tag.as_deref().unwrap_or("(per --tag)")),
    );
    let containers: Vec<&ContainerInfo> = report.map_or_else(Vec::new, |r| {
        r.hosts
            .iter()
            .flat_map(|h| h.containers.iter())
            .filter(|c| c.yoink_service.as_deref() == Some(name))
            .collect()
    });
    let running = containers.iter().filter(|c| c.is_running()).count();
    let total_hosts = report.map_or(0, |r| r.hosts.len());
    let running_str = format!("{running}/{total_hosts}");
    let health = summarize_health(&containers);
    let hosts: Vec<&str> = containers
        .iter()
        .filter(|c| c.is_running())
        .map(|c| c.host.as_str())
        .collect();
    let hosts_str = if hosts.is_empty() {
        "-".into()
    } else {
        hosts.join(", ")
    };
    Row::new(vec![
        Cell::from(name.to_string()),
        Cell::from(image_tag),
        Cell::from(running_str),
        Cell::from(health.to_string()).style(health_style(health)),
        Cell::from(hosts_str),
    ])
}

/// Reduce a service's container set to one of: `healthy`, `unhealthy`,
/// `starting`, `mixed`, `-`. `mixed` means at least two non-equal hints
/// among running containers — easier to spot at a glance than a single
/// "majority wins" rollup.
fn summarize_health(containers: &[&ContainerInfo]) -> &'static str {
    let mut hints: Vec<&str> = containers
        .iter()
        .filter(|c| c.is_running())
        .filter_map(|c| c.health_hint())
        .collect();
    hints.sort_unstable();
    hints.dedup();
    match hints.as_slice() {
        ["healthy"] => "healthy",
        ["unhealthy"] => "unhealthy",
        ["starting"] => "starting",
        [_] | [] => "-",
        _ => "mixed",
    }
}

impl ServiceDetailState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch to a new service. Caller is expected to follow with a
    /// refresh (or wait for the next dashboard tick to deliver one).
    pub fn set_service(&mut self, name: String) {
        if self.service.as_deref() != Some(&name) {
            self.rows.clear();
            self.table.select(None);
            self.loaded = false;
        }
        self.service = Some(name);
    }

    /// Apply data from a shared `StatusReport`, filtering down to the
    /// rows for the currently-selected service. `host_users` lets us
    /// reconstruct the `Host` (with ssh user) that the row drills into.
    pub fn apply(&mut self, report: Option<Arc<StatusReport>>, config: &Config) {
        let Some(name) = self.service.clone() else {
            return;
        };
        if let Some(r) = report {
            let mut rows = Vec::new();
            for host_status in &r.hosts {
                let user = config
                    .hosts
                    .iter()
                    .find(|h| h.address == host_status.host)
                    .map(|h| h.user.clone())
                    .unwrap_or_default();
                for c in &host_status.containers {
                    if c.yoink_service.as_deref() == Some(name.as_str()) {
                        rows.push(ServiceContainerRow {
                            host: Host {
                                user: user.clone(),
                                address: host_status.host.clone(),
                            },
                            container: c.clone(),
                        });
                    }
                }
            }
            self.rows = rows;
            self.loaded = true;
            clamp_selection(&mut self.table, self.rows.len());
        }
    }

    pub fn select_next(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_indices().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    pub fn selected_row(&self) -> Option<ServiceContainerRow> {
        let visible = self.visible_indices();
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .and_then(|src| self.rows.get(src))
            .cloned()
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let searchable = format!(
                    "{} {} {}",
                    r.host.address, r.container.name, r.container.state
                );
                self.filter.matches(&searchable).then_some(i)
            })
            .collect()
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let layout = pane_layout(area);
        let header_text = match &self.service {
            Some(name) => format!(
                "yoink service · {name} · {} container(s) · ↑↓ select · enter for logs",
                self.rows.len()
            ),
            None => "yoink service · (no service selected)".into(),
        };
        let header = Paragraph::new(header_text).style(bold());
        frame.render_widget(header, layout[0]);

        let widths = [
            Constraint::Length(18), // host
            Constraint::Length(28), // container
            Constraint::Length(10), // state
            Constraint::Length(10), // health
            Constraint::Length(10), // version
            Constraint::Min(20),    // status text
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());
        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from("(loading…)")])]
        } else if self.rows.is_empty() {
            vec![Row::new(vec![Cell::from("(no instances)")])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from("(no instances match filter)")])]
        } else {
            visible
                .iter()
                .map(|i| {
                    let r = &self.rows[*i];
                    let health = r.container.health_hint().unwrap_or("-");
                    Row::new(vec![
                        Cell::from(r.host.address.clone()),
                        Cell::from(r.container.name.clone()),
                        Cell::from(r.container.state.clone()).style(state_style(&r.container.state)),
                        Cell::from(health.to_string()).style(health_style(health)),
                        Cell::from(
                            r.container
                                .yoink_version
                                .clone()
                                .unwrap_or_else(|| "-".into()),
                        ),
                        Cell::from(r.container.status_text.clone()),
                    ])
                })
                .collect()
        };
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("version").style(bold()),
                Cell::from("status").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("instances"));
        frame.render_stateful_widget(table, layout[1], &mut self.table);

        let footer = filter_footer(
            &self.filter,
            "q quit · esc back · ↑↓ select · enter logs · ! shell · D debug",
        );
        frame.render_widget(footer, layout[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::HostStatus;
    use std::collections::BTreeMap;

    fn cfg() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: a, user: deploy }
  - { address: b, user: deploy }
services:
  - { name: api, image: img/api, tag: v1, run: {} }
  - { name: web, image: img/web, tag: v1, run: {} }
"#,
        )
        .unwrap()
    }

    fn report() -> StatusReport {
        let api_a = container("a", "api-aaa", "api", "running", Some("healthy"));
        let api_b = container("b", "api-bbb", "api", "running", Some("starting"));
        let web_a = container("a", "web-ccc", "web", "running", Some("healthy"));
        StatusReport {
            hosts: vec![
                HostStatus {
                    host: "a".into(),
                    containers: vec![api_a, web_a],
                },
                HostStatus {
                    host: "b".into(),
                    containers: vec![api_b],
                },
            ],
        }
    }

    fn container(
        host: &str,
        name: &str,
        service: &str,
        state: &str,
        health: Option<&str>,
    ) -> ContainerInfo {
        let status_text = match health {
            Some(h) => format!("Up ({h})"),
            None => "Up".into(),
        };
        ContainerInfo {
            host: host.into(),
            name: name.into(),
            state: state.into(),
            status_text,
            created_unix: None,
            yoink_service: Some(service.into()),
            yoink_version: Some("v1".into()),
            yoink_spec_hash: None,
            other_labels: BTreeMap::new(),
        }
    }

    #[test]
    fn services_state_select_next_clamps_to_last() {
        let mut s = ServicesState::new();
        s.apply(Some(report()), vec!["api".into(), "web".into()]);
        s.select_next();
        s.select_next();
        s.select_next(); // clamped
        assert_eq!(s.selected_service().as_deref(), Some("web"));
    }

    #[test]
    fn service_detail_filters_rows_by_service() {
        let mut d = ServiceDetailState::new();
        d.set_service("api".into());
        d.apply(Some(Arc::new(report())), &cfg());
        let names: Vec<String> = d.rows.iter().map(|r| r.container.name.clone()).collect();
        assert_eq!(names, vec!["api-aaa".to_string(), "api-bbb".to_string()]);
    }

    #[test]
    fn service_summary_marks_mixed_health_as_mixed() {
        // sample row: api has one healthy + one starting → mixed.
        let r = report();
        let containers: Vec<&ContainerInfo> = r
            .hosts
            .iter()
            .flat_map(|h| h.containers.iter())
            .filter(|c| c.yoink_service.as_deref() == Some("api"))
            .collect();
        assert_eq!(summarize_health(&containers), "mixed");
    }
}
