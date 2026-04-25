//! Dashboard pane: auto-refreshing service/host status grid with
//! per-container CPU% and memory snapshots.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::config::Config;
use crate::docker_ops::{ContainerStats, DockerOps, Host};
use crate::output::format_bytes;
use crate::status::StatusReport;

use super::ui::{bold, health_style, pane_layout};

pub struct DashboardRefresh {
    pub report: Option<StatusReport>,
    pub stats: HashMap<String, ContainerStats>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DashboardState {
    report: Option<StatusReport>,
    /// Keyed by `<host>/<container>` so the render layer can look up live
    /// stats alongside the container info from the listing.
    stats: HashMap<String, ContainerStats>,
    last_error: Option<String>,
    loaded: bool,
}

impl DashboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests + sync paths.
    #[cfg(test)]
    pub async fn refresh(&mut self, ops: &dyn DockerOps, config: &Config) {
        let data = fetch(ops, config).await;
        self.apply(data);
    }

    /// Apply background-fetched results.
    pub fn apply(&mut self, data: DashboardRefresh) {
        self.last_error = data.error;
        if let Some(report) = data.report {
            self.report = Some(report);
            self.stats = data.stats;
            self.loaded = true;
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, config: &Config) {
        let layout = pane_layout(frame.area());

        let header = Paragraph::new(format!("yoink dashboard · service={}", config.service.name))
            .style(bold());
        frame.render_widget(header, layout[0]);

        let rows = self.build_rows();
        let widths = [
            Constraint::Length(18), // host
            Constraint::Length(26), // container
            Constraint::Length(9),  // state
            Constraint::Length(10), // health
            Constraint::Length(10), // version
            Constraint::Length(7),  // cpu%
            Constraint::Length(20), // mem
            Constraint::Min(20),    // created
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("version").style(bold()),
                Cell::from("cpu").style(bold()),
                Cell::from("mem").style(bold()),
                Cell::from("created").style(bold()),
            ]))
            .block(Block::default().borders(Borders::ALL).title("status"));
        frame.render_widget(table, layout[1]);

        let footer_text = if let Some(err) = &self.last_error {
            format!("error: {err}  ·  q quit · r refresh · d dashboard · h hosts · l logs")
        } else {
            "q quit · r refresh · d dashboard · h hosts · l logs".to_string()
        };
        let footer = Paragraph::new(footer_text).style(if self.last_error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        });
        frame.render_widget(footer, layout[2]);
    }

    fn build_rows(&self) -> Vec<Row<'_>> {
        let Some(report) = &self.report else {
            return vec![Row::new(vec![Cell::from("(loading…)")])];
        };
        let mut rows: Vec<Row<'_>> = Vec::new();
        for host in &report.hosts {
            if host.containers.is_empty() {
                rows.push(Row::new(vec![
                    Cell::from(host.host.clone()),
                    Cell::from("(none)"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                ]));
                continue;
            }
            for c in &host.containers {
                let health = c.health_hint().unwrap_or("-");
                let stats = self.stats.get(&stats_key(&host.host, &c.name));
                let cpu = stats.map_or_else(|| "-".into(), |s| format!("{:.1}%", s.cpu_pct));
                let mem = stats.map_or_else(
                    || "-".into(),
                    |s| match s.mem_limit {
                        Some(limit) if limit > 0 => {
                            format!("{} / {}", format_bytes(s.mem_used), format_bytes(limit))
                        }
                        _ => format_bytes(s.mem_used),
                    },
                );
                rows.push(Row::new(vec![
                    Cell::from(host.host.clone()),
                    Cell::from(c.name.clone()),
                    Cell::from(c.state.clone()),
                    Cell::from(health.to_string()).style(health_style(health)),
                    Cell::from(c.yoink_version.clone().unwrap_or_else(|| "-".into())),
                    Cell::from(cpu),
                    Cell::from(mem),
                    Cell::from(c.created_at.clone()),
                ]));
            }
        }
        rows
    }
}

fn stats_key(host: &str, container: &str) -> String {
    format!("{host}/{container}")
}

/// Background-friendly fetch — owned inputs so the future is `'static + Send`.
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, config: Arc<Config>) -> DashboardRefresh {
    fetch(ops.as_ref(), config.as_ref()).await
}

async fn fetch(ops: &dyn DockerOps, config: &Config) -> DashboardRefresh {
    let report = match StatusReport::collect(ops, config).await {
        Ok(r) => r,
        Err(e) => {
            return DashboardRefresh {
                report: None,
                stats: HashMap::new(),
                error: Some(format!("{e:#}")),
            };
        }
    };
    let host_user_by_address: HashMap<String, String> = config
        .hosts
        .iter()
        .map(|h| (h.address.clone(), h.user.clone()))
        .collect();
    let stats_futs = report
        .hosts
        .iter()
        .flat_map(|h| {
            let host_addr = h.host.clone();
            let user = host_user_by_address
                .get(&host_addr)
                .cloned()
                .unwrap_or_default();
            h.containers
                .iter()
                .filter(|c| c.is_running())
                .map(move |c| {
                    let host = Host {
                        user: user.clone(),
                        address: host_addr.clone(),
                    };
                    let key = stats_key(&host_addr, &c.name);
                    let name = c.name.clone();
                    async move {
                        let result = ops.container_stats(&host, &name).await;
                        (key, result)
                    }
                })
        })
        .collect::<Vec<_>>();
    let stats_results = join_all(stats_futs).await;
    let mut stats = HashMap::new();
    for (key, result) in stats_results {
        if let Ok(s) = result {
            stats.insert(key, s);
        }
    }
    DashboardRefresh {
        report: Some(report),
        stats,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{ContainerInfo, ContainerStats, FakeDockerOps};
    use std::collections::BTreeMap;

    fn config() -> Config {
        Config::parse_str(
            r#"
[service]
name = "app-a"
image = "x"

[[hosts]]
address = "host-a"
user = "deploy"

[run]
port = 3000
"#,
        )
        .unwrap()
    }

    fn running_container() -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: "app-a-a1b2c3d".into(),
            state: "running".into(),
            status_text: "Up 1h (healthy)".into(),
            created_at: "2026-04-25 09:00".into(),
            yoink_service: Some("app-a".into()),
            yoink_version: Some("a1b2c3d".into()),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_collects_listing_then_stats_per_running_container() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 12.5,
            mem_used: 64 * 1024 * 1024,
            mem_limit: Some(512 * 1024 * 1024),
        }));
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.report.is_some());
        assert_eq!(state.stats.len(), 1);
        let s = state.stats.get("host-a/app-a-a1b2c3d").unwrap();
        assert!((s.cpu_pct - 12.5).abs() < f64::EPSILON);
        assert_eq!(s.mem_used, 64 * 1024 * 1024);
    }

    #[tokio::test]
    async fn refresh_skips_stats_for_non_running_containers() {
        let ops = FakeDockerOps::new();
        let mut stopped = running_container();
        stopped.state = "exited".into();
        ops.push_list_containers(Ok(vec![stopped]));
        // No push_container_stats — refresh shouldn't attempt one.
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.stats.is_empty());
    }

    #[tokio::test]
    async fn refresh_keeps_report_when_individual_stats_fails() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        // No stats response — fetch returns FakeExhausted, which we
        // tolerate; report is still populated, stats map stays empty.
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.report.is_some());
        assert!(state.stats.is_empty());
    }

    #[tokio::test]
    async fn refresh_captures_error_into_last_error_field() {
        let ops = FakeDockerOps::new();
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.last_error.is_some());
        assert!(state.report.is_none());
    }
}
