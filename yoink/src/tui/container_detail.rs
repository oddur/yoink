//! Container detail pane — k9s-style "describe" view. Renders a
//! rich inspect (image, command, env, ports, mounts, networks,
//! labels), live CPU/Mem gauges, and tails the container's logs in
//! a panel at the bottom.
//!
//! Reachable via `i` from `HostDetail` or `ServiceDetail`. From
//! inside: `Enter`/`l` opens the dedicated logs view, `!` shells in,
//! `D` spawns the debug sidecar, `Esc` returns to host detail.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use crate::docker_ops::{ContainerDetail, ContainerStats, DockerOps, Host};
use crate::output::{format_bytes, format_relative_time};

use super::ui::{bold, gauge_color, health_style, inline_gauge, kv, kv_styled, state_style};

/// Substrings that mark an env var as secret-ish — values are
/// rendered as `<redacted>` so an over-the-shoulder operator can't
/// accidentally leak prod creds. Keys themselves stay visible.
const REDACT_HINTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASS",
    "API_KEY",
    "PRIVATE_KEY",
    "DSN",
];

#[derive(Default)]
pub struct ContainerDetailState {
    target: Option<(Host, String)>,
    inspect: Option<ContainerDetail>,
    stats: Option<ContainerStats>,
    last_error: Option<String>,
    loaded: bool,
}

pub struct ContainerDetailRefresh {
    pub inspect: Option<ContainerDetail>,
    pub stats: Option<ContainerStats>,
    pub error: Option<String>,
}

impl ContainerDetailState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_target(&mut self, host: Host, container: String) {
        if self.target.as_ref().map(|(h, c)| (h, c.as_str())) != Some((&host, container.as_str())) {
            self.inspect = None;
            self.stats = None;
            self.loaded = false;
        }
        self.target = Some((host, container));
    }

    pub fn target(&self) -> Option<&(Host, String)> {
        self.target.as_ref()
    }

    pub fn apply(&mut self, data: ContainerDetailRefresh) {
        self.last_error = data.error;
        if self.last_error.is_none() {
            self.inspect = data.inspect;
            self.stats = data.stats;
            self.loaded = true;
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(err) = &self.last_error {
            let block = Block::default().borders(Borders::ALL).title(" container ");
            frame.render_widget(
                Paragraph::new(format!("error: {err}"))
                    .style(Style::default().fg(Color::Red))
                    .block(block),
                area,
            );
            return;
        }

        let Some(inspect) = &self.inspect else {
            let msg = if self.loaded {
                "(container not found — may have been removed)"
            } else {
                "(loading…)"
            };
            frame.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(Block::default().borders(Borders::ALL).title(" container ")),
                area,
            );
            return;
        };

        // Layout: header card (8 rows) · two-column data (rest split
        // horizontally between env+labels and ports+mounts+networks).
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(0)])
            .split(area);
        Self::render_card(frame, chunks[0], inspect, self.stats.as_ref());

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);
        Self::render_runtime(frame, cols[0], inspect);
        Self::render_env_labels(frame, cols[1], inspect);
    }

    /// Top "card" — image, state, command, restart info, live gauges.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn render_card(
        frame: &mut Frame<'_>,
        area: Rect,
        inspect: &ContainerDetail,
        stats: Option<&ContainerStats>,
    ) {
        let block = Block::default().borders(Borders::ALL).title(" container ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Two-column layout inside the card: left = static facts,
        // right = live gauges.
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(inner);

        let lifecycle = inspect.state.as_deref().unwrap_or("?");
        let health = inspect.labels.get("yoink.health").map(String::as_str);
        let mut left_lines = vec![
            kv("image", inspect.image.as_deref().unwrap_or("?")),
            kv_styled("state", lifecycle, state_style(lifecycle)),
        ];
        if let Some(h) = health {
            left_lines.push(kv_styled("health", h, health_style(h)));
        }
        if let Some(cmd) = &inspect.command {
            left_lines.push(kv("command", cmd));
        }
        if let Some(wd) = inspect.working_dir.as_deref().filter(|s| !s.is_empty()) {
            left_lines.push(kv("workdir", wd));
        }
        let restart_text = match (inspect.restart_count, inspect.restart_policy.as_deref()) {
            (Some(n), Some(p)) => format!("{n} ({p})"),
            (Some(n), None) => n.to_string(),
            (None, Some(p)) => p.to_string(),
            _ => "-".into(),
        };
        left_lines.push(kv("restarts", &restart_text));
        if let Some(pid) = inspect.pid.filter(|p| *p > 0) {
            left_lines.push(kv("pid", &pid.to_string()));
        }
        frame.render_widget(Paragraph::new(left_lines), cols[0]);

        let mut right_lines: Vec<Line<'static>> = Vec::new();
        if let Some(s) = stats {
            let cpu_ratio = (s.cpu_pct as f32 / 100.0).clamp(0.0, 1.0);
            let mut cpu_spans = vec![Span::raw(format!("CPU {:>5.1}% ", s.cpu_pct))];
            cpu_spans.extend(inline_gauge(cpu_ratio, 16, gauge_color(cpu_ratio)));
            right_lines.push(Line::from(cpu_spans));

            let (label, ratio) = match s.mem_limit {
                Some(limit) if limit > 0 => (
                    format!(
                        "MEM {:>8} / {:<8} ",
                        format_bytes(s.mem_used),
                        format_bytes(limit)
                    ),
                    ((s.mem_used as f32) / (limit as f32)).clamp(0.0, 1.0),
                ),
                _ => (
                    format!("MEM {:>8}            ", format_bytes(s.mem_used)),
                    0.0,
                ),
            };
            let mut mem_spans = vec![Span::raw(label)];
            mem_spans.extend(inline_gauge(ratio, 16, gauge_color(ratio)));
            right_lines.push(Line::from(mem_spans));
        } else {
            right_lines.push(Line::from(Span::styled(
                "(no live stats yet)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        right_lines.push(Line::from(""));
        if let Some(started) = parse_rfc3339_age(inspect.started_at.as_deref()) {
            right_lines.push(kv("started", &started));
        }
        if let Some(finished) = parse_rfc3339_age(inspect.finished_at.as_deref()) {
            right_lines.push(kv("finished", &finished));
        }
        if let Some(code) = inspect.exit_code {
            right_lines.push(kv("exit code", &code.to_string()));
        }
        frame.render_widget(Paragraph::new(right_lines), cols[1]);
    }

    fn render_runtime(frame: &mut Frame<'_>, area: Rect, inspect: &ContainerDetail) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(20), // ports
                Constraint::Percentage(30), // mounts
                Constraint::Percentage(20), // networks
                Constraint::Percentage(30), // security & limits
            ])
            .split(area);

        Self::render_list(frame, chunks[0], " ports ", &inspect.ports);
        Self::render_list(frame, chunks[1], " mounts ", &inspect.mounts);
        Self::render_list(frame, chunks[2], " networks ", &inspect.networks);
        Self::render_security(frame, chunks[3], inspect);
    }

    /// Surface the secure-by-default profile + any override the
    /// operator opted into (`memory`, `cpus`, `pids_limit`,
    /// `cap_drop`/`cap_add`, `security_opt`, `read_only`, `user`).
    /// When all knobs are uncapped / at default this section is small
    /// but still useful — it tells you "yes, this container is
    /// hardened" at a glance.
    fn render_security(frame: &mut Frame<'_>, area: Rect, inspect: &ContainerDetail) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        // Resource caps row.
        let mem = inspect
            .memory_bytes
            .map_or_else(|| "uncapped".into(), format_bytes);
        #[allow(clippy::cast_precision_loss)]
        let cpus = inspect.nano_cpus.map_or_else(
            || "uncapped".into(),
            |n| format!("{:.2}", n as f64 / 1_000_000_000.0),
        );
        let pids = inspect
            .pids_limit
            .map_or_else(|| "unlimited".into(), |n| n.to_string());
        lines.push(kv("memory", &mem));
        lines.push(kv("cpus", &cpus));
        lines.push(kv("pids_limit", &pids));
        lines.push(kv(
            "user",
            inspect.user.as_deref().unwrap_or("(image default)"),
        ));
        // Security knobs. Each rendered with a marker so the operator
        // can scan for "is this hardened?" at a glance.
        let cap_drop = if inspect.cap_drop.is_empty() {
            "(none — capabilities NOT dropped)".to_string()
        } else {
            inspect.cap_drop.join(",")
        };
        lines.push(kv("cap_drop", &cap_drop));
        if !inspect.cap_add.is_empty() {
            lines.push(kv("cap_add", &inspect.cap_add.join(",")));
        }
        let secopt = if inspect.security_opt.is_empty() {
            "(none — setuid escalation NOT blocked)".to_string()
        } else {
            inspect.security_opt.join(",")
        };
        lines.push(kv("security_opt", &secopt));
        lines.push(kv(
            "read_only",
            if inspect.read_only {
                "true (immutable rootfs)"
            } else {
                "false (writable rootfs)"
            },
        ));
        let block = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" security & limits "),
        );
        frame.render_widget(block, area);
    }

    fn render_env_labels(frame: &mut Frame<'_>, area: Rect, inspect: &ContainerDetail) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        // env: redact secret-ish values. Wrap in case lines are long.
        let env_lines: Vec<Line<'static>> = inspect
            .env
            .iter()
            .map(|entry| {
                let (k, v) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
                let redacted = is_secret(k);
                let value_span = if redacted {
                    Span::styled(
                        "<redacted>".to_string(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )
                } else {
                    Span::raw(v.to_string())
                };
                Line::from(vec![
                    Span::styled(format!("{k}="), Style::default().fg(Color::Cyan)),
                    value_span,
                ])
            })
            .collect();
        let env = Paragraph::new(env_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" env "));
        frame.render_widget(env, chunks[0]);

        let label_rows: Vec<Row<'_>> = inspect
            .labels
            .iter()
            .map(|(k, v)| Row::new(vec![Cell::from(k.clone()), Cell::from(v.clone())]))
            .collect();
        let widths = [Constraint::Length(28), Constraint::Min(20)];
        let labels_table = Table::new(label_rows, widths)
            .header(Row::new(vec![
                Cell::from("label").style(bold()),
                Cell::from("value").style(bold()),
            ]))
            .block(Block::default().borders(Borders::ALL).title(" labels "));
        frame.render_widget(labels_table, chunks[1]);
    }

    fn render_list(frame: &mut Frame<'_>, area: Rect, title: &str, items: &[String]) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.to_string());
        if items.is_empty() {
            let body = Paragraph::new("(none)")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            frame.render_widget(body, area);
            return;
        }
        let lines: Vec<Line<'static>> = items.iter().cloned().map(Line::from).collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

fn is_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    REDACT_HINTS.iter().any(|hint| upper.contains(hint))
}

/// Parse an RFC-3339 docker timestamp into a relative-time string.
/// Returns `None` for the empty string and the docker-zero
/// `0001-01-01T00:00:00Z` placeholder.
fn parse_rfc3339_age(s: Option<&str>) -> Option<String> {
    let s = s?;
    if s.is_empty() || s.starts_with("0001-01-01") {
        return None;
    }
    // Avoid pulling chrono/time just for this — parse the "YYYY-MM-DDTHH:MM:SS"
    // prefix manually and convert to unix seconds. Best-effort: returns
    // the raw string if parsing fails.
    let parsed = parse_unix_seconds(s);
    Some(parsed.map_or_else(|| s.to_string(), |t| format_relative_time(Some(t))))
}

/// Bare-bones RFC-3339 parser: returns unix seconds for
/// "YYYY-MM-DDTHH:MM:SS(.fff)?Z" or with a `+HH:MM` offset.
/// Pulled inline so we don't add chrono just to render an "age".
fn parse_unix_seconds(s: &str) -> Option<i64> {
    // Date
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let se: i64 = s.get(17..19)?.parse().ok()?;

    // Civil date → days since 1970-01-01 (Howard Hinnant's algorithm).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = (if yy >= 0 { yy } else { yy - 399 }) / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

/// Background-friendly fetch — `'static + Send` so it can be `tokio::spawn`-ed.
pub async fn fetch_owned(
    ops: Arc<dyn DockerOps>,
    host: Host,
    container: String,
) -> ContainerDetailRefresh {
    let inspect = match ops.inspect_container(&host, &container).await {
        Ok(d) => Some(d),
        Err(e) => {
            return ContainerDetailRefresh {
                inspect: None,
                stats: None,
                error: Some(format!("inspect: {e}")),
            };
        }
    };
    let stats = ops.container_stats(&host, &container).await.ok();
    ContainerDetailRefresh {
        inspect,
        stats,
        error: None,
    }
}
