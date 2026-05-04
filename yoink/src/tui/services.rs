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
use crate::secrets::SecretsBundle;
use crate::status::StatusReport;

use super::ui::{
    FilterState, bold, clamp_selection, filter_footer, health_style, pane_layout,
    render_drift_cell, state_style,
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
    /// `report = None` means the underlying fetch failed (host
    /// unreachable, etc.); we still flip `loaded = true` so the pane
    /// renders rows from the config (with running=0/N) instead of
    /// hanging on `(loading…)` indefinitely.
    pub fn apply(&mut self, report: Option<StatusReport>, service_names: Vec<String>) {
        self.service_names = service_names;
        self.loaded = true;
        if let Some(r) = report {
            self.report = Some(r);
        }
        // Clamp against the *filtered* count — table.selected() indexes
        // into visible_indices(), not the full service_names list.
        let n = self.visible_indices().len();
        clamp_selection(&mut self.table, n);
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

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        config: &Config,
        forwards: &super::pf::PortForwardState,
        vscode: &super::vscode::VscodeSessionState,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
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
            vec![Row::new(vec![Cell::from(super::ui::loading_line(
                throbber,
            ))])]
        } else if visible.is_empty() && !self.service_names.is_empty() {
            vec![Row::new(vec![Cell::from("(no services match filter)")])]
        } else {
            visible
                .iter()
                .map(|i| {
                    build_service_row(
                        &self.service_names[*i],
                        config,
                        self.report.as_ref(),
                        forwards,
                        vscode,
                    )
                })
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
    forwards: &super::pf::PortForwardState,
    vscode: &super::vscode::VscodeSessionState,
) -> Row<'a> {
    let cfg_service = config.services.iter().find(|s| s.name == name);
    let image_tag = cfg_service.map_or_else(
        || "?".into(),
        |s| match s.tag.as_deref() {
            Some(t) => crate::docker::image_reference(&s.image, t),
            None => format!("{}:(per --tag)", s.image),
        },
    );
    let containers: Vec<&ContainerInfo> = report.map_or_else(Vec::new, |r| {
        r.hosts
            .iter()
            .flat_map(|h| h.containers.iter())
            .filter(|c| c.yoink_service.as_deref() == Some(name))
            .collect()
    });
    let running = containers.iter().filter(|c| c.is_running()).count();
    // Expected = replicas × real hosts the service is configured to
    // run on. The synthetic `local` host the TUI injects for
    // read-only browsing must be excluded — including it inflates
    // every service's denominator (e.g. 1-replica on 1 host → 1/2).
    let expected = cfg_service.map_or(running, |s| {
        s.applicable_hosts(&config.hosts)
            .iter()
            .filter(|h| h.address != crate::docker_ops::Host::LOCAL_ADDRESS)
            .count()
            * s.run.replicas as usize
    });
    let running_str = format!("{running}/{expected}");
    let health = summarize_health(&containers);
    // Dedup hosts: with replicas > 1 every replica reports the same
    // host, so the column would otherwise read "host-a, host-a".
    let mut hosts: Vec<&str> = containers
        .iter()
        .filter(|c| c.is_running())
        .map(|c| c.host.as_str())
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    let hosts_str = if hosts.is_empty() {
        "-".into()
    } else {
        hosts.join(", ")
    };
    let name_cell = build_marker_cell(name, forwards.is_service_forwarded(name), vscode);
    Row::new(vec![
        name_cell,
        Cell::from(image_tag),
        Cell::from(running_str),
        Cell::from(health.to_string()).style(health_style(health)),
        Cell::from(hosts_str),
    ])
}

/// Marker-prefixed cell for a service / container row. Pass the
/// already-decided `pf_active` (callers either ask the per-service
/// or per-container predicate); vscode sessions are per-service so
/// the helper looks them up itself. `↦` = port-forward active,
/// `◊` = vscode session active. Markers stack; cyan when any active.
pub(super) fn build_marker_cell(
    service: &str,
    pf_active: bool,
    vscode: &super::vscode::VscodeSessionState,
) -> Cell<'static> {
    let vs = vscode.is_service_active(service);
    if !pf_active && !vs {
        return Cell::from(service.to_string());
    }
    let mut prefix = String::with_capacity(4);
    if pf_active {
        prefix.push('↦');
    }
    if vs {
        prefix.push('◊');
    }
    Cell::from(format!("{prefix} {service}"))
        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan))
}

/// Reduce a service's container set to one of: `healthy`, `unhealthy`,
/// `starting`, `mixed`, `-`. `mixed` means at least two non-equal hints
/// among running containers — easier to spot at a glance than a single
/// "majority wins" rollup.
fn summarize_health(containers: &[&ContainerInfo]) -> &'static str {
    let mut hints: Vec<&str> = containers
        .iter()
        .filter(|c| c.is_running())
        .filter_map(|c| c.health_hint().map(crate::docker_ops::HealthStatus::as_str))
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
    /// `report = None` means the underlying fetch failed; we still
    /// flip `loaded = true` so the pane renders `(no instances)`
    /// instead of looping on `(loading…)` indefinitely.
    pub fn apply(&mut self, report: Option<Arc<StatusReport>>, config: &Config) {
        let Some(name) = self.service.clone() else {
            return;
        };
        self.loaded = true;
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
            // Clamp against the *filtered* count — `table.selected()`
            // indexes into `visible_indices()`, not `self.rows`.
            // `clamp_selection` also picks row 0 when none was set,
            // so per-row actions work on first apply.
            let n = self.visible_indices().len();
            clamp_selection(&mut self.table, n);
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

    /// Currently-selected service name, if any. Used by the
    /// reconcile flow which acts on the service this view targets.
    #[must_use]
    pub fn current_service(&self) -> Option<&str> {
        self.service.as_deref()
    }

    /// First running container's tag for this view's service —
    /// used as the fallback tag for reconcile when no
    /// config-pinned tag exists.
    #[must_use]
    pub fn running_tag_for_service(&self, service: &str) -> Option<String> {
        if self.service.as_deref() != Some(service) {
            return None;
        }
        self.rows.iter().find_map(|r| {
            if r.container.is_running() {
                r.container.yoink_version.clone()
            } else {
                None
            }
        })
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

    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        config: &Config,
        secrets: Option<&SecretsBundle>,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let layout = pane_layout(area);
        let header_text = match &self.service {
            Some(name) => {
                // Surface the xcaddy plugin set on the synthesized
                // proxy row so an operator can see what's compiled
                // into the running caddy without docker-inspecting
                // the image. Inlined into the existing single-line
                // header (pane_layout reserves one row).
                let plugins = if name == crate::proxy::PROXY_SERVICE_NAME {
                    xcaddy_plugin_list(config)
                } else {
                    None
                };
                match plugins {
                    Some(p) => format!(
                        "yoink service · {name} · {} container(s) · plugins: {p} · ↑↓ select",
                        self.rows.len()
                    ),
                    None => format!(
                        "yoink service · {name} · {} container(s) · ↑↓ select · enter for logs",
                        self.rows.len()
                    ),
                }
            }
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
            Constraint::Length(7),  // drift
            Constraint::Min(20),    // status text
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());
        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(
                throbber,
            ))])]
        } else if self.rows.is_empty() {
            vec![Row::new(vec![Cell::from("(no instances)")])]
        } else if visible.is_empty() {
            vec![Row::new(vec![Cell::from("(no instances match filter)")])]
        } else {
            visible
                .iter()
                .map(|i| {
                    let r = &self.rows[*i];
                    let health = r
                        .container
                        .health_hint()
                        .map_or("-", crate::docker_ops::HealthStatus::as_str);
                    Row::new(vec![
                        Cell::from(r.host.address.clone()),
                        Cell::from(r.container.name.clone()),
                        Cell::from(r.container.state.as_str())
                            .style(state_style(r.container.state)),
                        Cell::from(health.to_string()).style(health_style(health)),
                        Cell::from(
                            r.container
                                .yoink_version
                                .clone()
                                .unwrap_or_else(|| "-".into()),
                        ),
                        render_drift_cell(&r.container, config, secrets),
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
                Cell::from("drift").style(bold()),
                Cell::from("status").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("instances"));
        frame.render_stateful_widget(table, layout[1], &mut self.table);

        let footer = filter_footer(
            &self.filter,
            "q quit · esc back · ↑↓ select · enter logs · ! shell · f forward · v vscode · D debug",
        );
        frame.render_widget(footer, layout[2]);
    }
}

/// Comma-joined plugin list for the proxy detail header. `None` when
/// `proxy.xcaddy:` isn't configured (so the operator's running stock
/// caddy:2 / a `proxy.image:` BYO build, neither of which yoink can
/// introspect for plugins). Sorted to match what xcaddy bakes in. The
/// `github.com/` prefix is stripped from each entry — the host part is
/// noise on the only-takes-one-row TUI header, and `caddyserver/x`
/// vs `mholt/y` is what an operator actually wants to see.
fn xcaddy_plugin_list(config: &Config) -> Option<String> {
    let plugins = config.proxy.as_ref()?.xcaddy.as_ref()?.sorted_plugins();
    if plugins.is_empty() {
        return None;
    }
    Some(
        plugins
            .iter()
            .map(|p| p.strip_prefix("github.com/").unwrap_or(p))
            .collect::<Vec<_>>()
            .join(", "),
    )
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

    #[test]
    fn xcaddy_plugin_list_none_when_no_proxy() {
        assert!(xcaddy_plugin_list(&cfg()).is_none());
    }

    #[test]
    fn xcaddy_plugin_list_sorted_csv_with_github_prefix_stripped() {
        let cfg = Config::parse_str(
            r#"
hosts: [{ address: a, user: deploy }]
proxy:
  email: ops@example.com
  xcaddy:
    plugins:
      - github.com/zeta/last
      - github.com/alpha/first
services:
  - { name: api, image: img/api, tag: v1, domain: api.example.com, run: { port: 8080 } }
"#,
        )
        .unwrap();
        let listed = xcaddy_plugin_list(&cfg).expect("plugins listed");
        assert!(
            !listed.contains("github.com/"),
            "github.com/ stripped: {listed}"
        );
        assert!(listed.contains("alpha/first"));
        assert!(listed.contains("zeta/last"));
        assert!(
            listed.find("alpha/first").unwrap() < listed.find("zeta/last").unwrap(),
            "sorted alphabetically: {listed}"
        );
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
            image: String::new(),
            state: state.into(),
            status_text,
            created_unix: None,
            yoink_service: Some(service.into()),
            yoink_version: Some("v1".into()),
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
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
