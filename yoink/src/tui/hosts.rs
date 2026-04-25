//! Hosts pane: per-host preflight + Docker daemon stats with live
//! utilization (CPU% / mem) summed across every running container on the
//! host. Background-refreshes regardless of which pane is active so
//! switching to `Hosts` is instantaneous.

use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::config::{Config, HostConfig};
use crate::docker_ops::{DockerOps, DockerVersion, Host, HostInfo};
use crate::output::format_bytes;

use super::ui::{bold, clamp_selection, filter_footer, inline_gauge, pane_layout, FilterState};

#[derive(Default)]
pub struct HostsState {
    rows: Vec<HostRow>,
    table: TableState,
    loaded: bool,
    pub filter: FilterState,
}

pub struct HostRow {
    address: String,
    user: String,
    status: HostStatus,
}

pub enum HostStatus {
    Ok(Box<OkRow>),
    Err(String),
}

pub struct OkRow {
    version: DockerVersion,
    info: Option<HostInfo>,
    usage: Option<HostUsage>,
}

/// Aggregate of `docker stats` across every running container on the host.
struct HostUsage {
    /// Sum of `cpu_pct` values; max ≈ `n_cpu * 100`.
    cpu_pct: f64,
    /// Sum of memory in bytes used by all running containers.
    mem_used: i64,
}

impl HostsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests + sync paths: `apply(fetch().await)`.
    #[cfg(test)]
    pub async fn refresh(&mut self, ops: &dyn DockerOps, config: &Config) {
        let rows = fetch_rows(ops, &config.hosts).await;
        self.apply(rows);
    }

    /// Replace rows with a freshly-fetched batch and rebalance selection.
    pub fn apply(&mut self, rows: Vec<HostRow>) {
        self.rows = rows;
        self.loaded = true;
        let n = self.visible_rows().len();
        clamp_selection(&mut self.table, n);
    }

    pub fn select_next(&mut self) {
        let n = self.visible_rows().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_rows().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    /// `(user, address)` of the currently-selected host, if any.
    pub fn selected_host(&self) -> Option<Host> {
        let visible = self.visible_rows();
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .map(|r| Host {
                user: r.user.clone(),
                address: r.address.clone(),
            })
    }

    fn visible_rows(&self) -> Vec<&HostRow> {
        self.rows
            .iter()
            .filter(|r| {
                let target = format!("{}@{}", r.user, r.address);
                self.filter.matches(&target)
            })
            .collect()
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, _config: &Config) {
        let layout = pane_layout(area);

        let header = Paragraph::new("yoink hosts · ↑↓ select · enter for detail").style(bold());
        frame.render_widget(header, layout[0]);

        let visible_count = self.visible_rows().len();
        clamp_selection(&mut self.table, visible_count);
        let visible = self.visible_rows();
        let rows: Vec<Row<'_>> = if !self.loaded {
            vec![Row::new(vec![Cell::from("(loading…)")])]
        } else if visible.is_empty() && !self.rows.is_empty() {
            vec![Row::new(vec![Cell::from("(no hosts match filter)")])]
        } else if self.rows.is_empty() {
            vec![Row::new(vec![Cell::from("(no hosts configured)")])]
        } else {
            visible.iter().copied().map(host_row).collect()
        };

        let widths = [
            Constraint::Length(22), // ssh target
            Constraint::Length(11), // status
            Constraint::Length(8),  // server
            Constraint::Length(24), // cpu (used / total cores + gauge)
            Constraint::Length(34), // mem (used / total + gauge)
            Constraint::Length(11), // running/total containers
            Constraint::Length(7),  // images
            Constraint::Min(20),    // platform / error
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("ssh target").style(bold()),
                Cell::from("status").style(bold()),
                Cell::from("server").style(bold()),
                Cell::from("cpu (cores)").style(bold()),
                Cell::from("memory").style(bold()),
                Cell::from("ctr run/all").style(bold()),
                Cell::from("images").style(bold()),
                Cell::from("platform / error").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("hosts"));
        frame.render_stateful_widget(table, layout[1], &mut self.table);

        let footer = filter_footer(
            &self.filter,
            "q quit · ↑↓ select · enter detail · r refresh · ? help",
        );
        frame.render_widget(footer, layout[2]);
    }
}

/// Background-friendly fetch: takes owned inputs so the resulting future
/// is `'static + Send` and can be `tokio::spawn`-ed.
pub async fn fetch_rows_owned(ops: Arc<dyn DockerOps>, hosts: Vec<HostConfig>) -> Vec<HostRow> {
    fetch_rows(ops.as_ref(), &hosts).await
}

async fn fetch_rows(ops: &dyn DockerOps, hosts: &[HostConfig]) -> Vec<HostRow> {
    let futs = hosts.iter().map(|h| async move {
        let host = Host::from(h);
        let version = ops.version(&host).await;
        if let Err(e) = version {
            return (None, None, None, Some(format!("{e:#}")));
        }
        let version = version.unwrap();
        let info = ops.host_info(&host).await.ok();
        let usage = host_usage(ops, &host).await;
        (Some(version), info, usage, None)
    });
    let results = join_all(futs).await;
    hosts
        .iter()
        .zip(results)
        .map(|(cfg, (version, info, usage, err))| HostRow {
            address: cfg.address.clone(),
            user: cfg.user.clone(),
            status: match (version, err) {
                (Some(version), _) => HostStatus::Ok(Box::new(OkRow {
                    version,
                    info,
                    usage,
                })),
                (None, Some(e)) => HostStatus::Err(e),
                (None, None) => HostStatus::Err("unknown error".into()),
            },
        })
        .collect()
}

/// List every running container on `host` and sum its stats. Returns None
/// if listing fails; per-container stat failures are silently dropped so
/// one wedged container can't blank the whole host row.
async fn host_usage(ops: &dyn DockerOps, host: &Host) -> Option<HostUsage> {
    let containers = ops.list_running_containers(host).await.ok()?;
    let stat_futs = containers.iter().map(|c| {
        let name = c.name.clone();
        async move { ops.container_stats(host, &name).await }
    });
    let results = join_all(stat_futs).await;
    let mut cpu_pct = 0.0;
    let mut mem_used = 0_i64;
    for r in results.into_iter().flatten() {
        cpu_pct += r.cpu_pct;
        mem_used = mem_used.saturating_add(r.mem_used);
    }
    Some(HostUsage { cpu_pct, mem_used })
}

fn host_row(r: &HostRow) -> Row<'static> {
    let target = format!("{}@{}", r.user, r.address);
    match &r.status {
        HostStatus::Ok(ok) => {
            let version = &ok.version;
            let info = ok.info.as_ref();
            let usage = ok.usage.as_ref();
            let server = version.server_version.clone().unwrap_or_else(|| "?".into());
            let platform = format!(
                "{}/{}",
                version.os.as_deref().unwrap_or("?"),
                version.arch.as_deref().unwrap_or("?")
            );
            let cpu_cell = cell_cpu(info, usage);
            let mem_cell = cell_mem(info, usage);
            let (containers, images) = info.map_or_else(
                || ("?".into(), "?".into()),
                |i| {
                    (
                        format!(
                            "{}/{}",
                            i.containers_running.unwrap_or(0),
                            i.containers.unwrap_or(0)
                        ),
                        i.images.map_or_else(|| "?".into(), |n| n.to_string()),
                    )
                },
            );
            Row::new(vec![
                Cell::from(target),
                Cell::from("connected").style(Style::default().fg(Color::Green)),
                Cell::from(server),
                cpu_cell,
                mem_cell,
                Cell::from(containers),
                Cell::from(images),
                Cell::from(platform),
            ])
        }
        HostStatus::Err(e) => Row::new(vec![
            Cell::from(target),
            Cell::from("error").style(Style::default().fg(Color::Red)),
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("-"),
            Cell::from(truncate(e, 60)),
        ]),
    }
}

/// Cell renders `<used> / <total>` cores plus a bracketed gauge of
/// host CPU saturation against the daemon's reported core count.
/// Falls back to text-only when either side is missing.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn cell_cpu(info: Option<&HostInfo>, usage: Option<&HostUsage>) -> Cell<'static> {
    let total = info.and_then(|i| i.n_cpu);
    match (usage, total) {
        (Some(u), Some(t)) => {
            let used = u.cpu_pct / 100.0;
            let ratio = ((used as f32) / (t as f32)).clamp(0.0, 1.0);
            let mut spans = vec![Span::raw(format!("{used:>4.2} / {t:<3} "))];
            spans.extend(inline_gauge(ratio, 10, gauge_color(ratio)));
            Cell::from(Line::from(spans))
        }
        (None, Some(t)) => Cell::from(format!("- / {t}")),
        (Some(u), None) => Cell::from(format!("{:.2}", u.cpu_pct / 100.0)),
        (None, None) => Cell::from("-").style(Style::default().fg(Color::DarkGray)),
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn cell_mem(info: Option<&HostInfo>, usage: Option<&HostUsage>) -> Cell<'static> {
    let total = info.and_then(|i| i.mem_total);
    match (usage, total) {
        (Some(u), Some(t)) => {
            let ratio = ((u.mem_used as f32) / (t as f32)).clamp(0.0, 1.0);
            let label = format!(
                "{:>8} / {:<8} ",
                format_bytes(u.mem_used),
                format_bytes(t)
            );
            let mut spans = vec![Span::raw(label)];
            spans.extend(inline_gauge(ratio, 10, gauge_color(ratio)));
            Cell::from(Line::from(spans))
        }
        (None, Some(t)) => Cell::from(format!("- / {}", format_bytes(t))),
        (Some(u), None) => Cell::from(format_bytes(u.mem_used)),
        (None, None) => Cell::from("-").style(Style::default().fg(Color::DarkGray)),
    }
}

/// Green / yellow / red threshold for a 0..=1 gauge.
fn gauge_color(ratio: f32) -> Color {
    if ratio < 0.60 {
        Color::Green
    } else if ratio < 0.85 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{
        ContainerInfo, ContainerStats, DockerError, DockerVersion, FakeDockerOps, HostInfo,
    };
    use std::collections::BTreeMap;

    fn config_two_hosts() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
  - { address: host-b, user: root }
services:
  - name: yoink-test
    image: nginx
    tag: alpine
    run: { port: 80, healthcheck_path: / }
"#,
        )
        .unwrap()
    }

    fn version() -> DockerVersion {
        DockerVersion {
            server_version: Some("29.1.3".into()),
            api_version: Some("1.52".into()),
            os: Some("linux".into()),
            arch: Some("amd64".into()),
        }
    }

    fn info() -> HostInfo {
        HostInfo {
            n_cpu: Some(8),
            mem_total: Some(32 * 1024 * 1024 * 1024),
            containers: Some(12),
            containers_running: Some(3),
            images: Some(20),
            kernel: Some("6.1.0".into()),
            operating_system: Some("Debian GNU/Linux 12".into()),
        }
    }

    fn container(name: &str) -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: name.into(),
            state: "running".into(),
            status_text: "Up".into(),
            created_unix: None,
            yoink_service: None,
            yoink_version: None,
            yoink_spec_hash: None,
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_aggregates_usage_per_host() {
        let ops = FakeDockerOps::new();
        // host-a: version + info + list_running(2) + stats x2
        ops.push_version(Ok(version()));
        ops.push_host_info(Ok(info()));
        ops.push_list_containers(Ok(vec![container("a"), container("b")]));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 50.0,
            mem_used: 100 * 1024 * 1024,
            mem_limit: None,
        }));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 25.0,
            mem_used: 200 * 1024 * 1024,
            mem_limit: None,
        }));
        // host-b: version errors → no further calls expected
        ops.push_version(Err(DockerError::FakeExhausted("test")));

        let mut state = HostsState::new();
        state.refresh(&ops, &config_two_hosts()).await;
        assert_eq!(state.rows.len(), 2);
        match &state.rows[0].status {
            HostStatus::Ok(ok) => {
                let u = ok.usage.as_ref().expect("usage present");
                assert!(ok.info.is_some());
                assert!((u.cpu_pct - 75.0).abs() < f64::EPSILON);
                assert_eq!(u.mem_used, 300 * 1024 * 1024);
            }
            HostStatus::Err(e) => panic!("expected ok+usage, got Err({e})"),
        }
        assert!(matches!(state.rows[1].status, HostStatus::Err(_)));
    }

    #[test]
    fn truncate_long_strings() {
        assert_eq!(truncate("short", 10), "short");
        let long = "x".repeat(80);
        let t = truncate(&long, 30);
        assert_eq!(t.chars().count(), 30);
        assert!(t.ends_with('…'));
    }
}
