//! Container detail pane — k9s-style "describe" view. Renders a
//! rich inspect (image, command, env, ports, mounts, networks,
//! labels), live CPU/Mem gauges, and tails the container's logs in
//! a panel at the bottom.
//!
//! Reachable via `i` from `HostDetail` or `ServiceDetail`. From
//! inside: `Enter`/`l` opens the dedicated logs view, `!` shells in,
//! `D` spawns the debug sidecar, `Esc` returns to host detail.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap,
};

use crate::docker_ops::{ContainerDetail, ContainerStats, DockerOps, Host, ProcessTable};
use crate::output::{format_bytes, format_relative_time};

use super::ui::{bold, gauge_color, health_style, inline_gauge, kv, kv_styled, state_style};

/// Total wall-clock window the history charts cover. Picked to be long
/// enough that an operator can correlate "I deployed X" with "memory
/// climbed for 4 minutes" without having to scroll, but short enough
/// that the underlying ring buffer stays cheap (≈150 samples at the 2s
/// fast-tick).
const HISTORY_WINDOW_SECS: f64 = 300.0;
/// Cap on samples retained in `StatsHistory` — generous overhead for
/// the case where the operator's terminal stays open longer than the
/// fast tick can keep up. At ~1 sample / second this is ~10 minutes
/// worth of headroom; older points are dropped.
const HISTORY_CAP: usize = 600;

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

/// Bounded ring of `(elapsed_seconds, value)` pairs covering at most
/// [`HISTORY_WINDOW_SECS`]. `started` anchors elapsed-time so the chart
/// keeps a stable x-axis as samples slide off.
#[derive(Default)]
pub struct StatsHistory {
    started: Option<Instant>,
    cpu_pct: VecDeque<(f64, f64)>,
    /// Memory utilization as a 0..=100 percentage when a memory cap
    /// is set; absolute MB when uncapped (chart auto-scales the y-axis
    /// in that case).
    mem_pct: VecDeque<(f64, f64)>,
    /// Cumulative bytes — the chart renderer diffs successive samples
    /// to derive a per-second rate.
    net_rx: VecDeque<(f64, f64)>,
    net_tx: VecDeque<(f64, f64)>,
    /// True when memory is uncapped on the container — the y-axis on
    /// the mem chart switches to "MB" labelling instead of "0–100%".
    mem_uncapped: bool,
    /// Last absolute mem bytes (for the uncapped case so the y-axis
    /// max can grow).
    mem_max_bytes: i64,
}

impl StatsHistory {
    fn now_elapsed(&mut self) -> f64 {
        let started = self.started.get_or_insert_with(Instant::now);
        started.elapsed().as_secs_f64()
    }

    /// Record a fresh sample. Drops points that fall outside the
    /// history window so the chart keeps a fixed time horizon.
    pub fn push(&mut self, stats: &ContainerStats) {
        let t = self.now_elapsed();
        self.cpu_pct.push_back((t, stats.cpu_pct.max(0.0)));

        match stats.mem_limit {
            Some(limit) if limit > 0 => {
                self.mem_uncapped = false;
                #[allow(clippy::cast_precision_loss)]
                let pct = (stats.mem_used as f64 / limit as f64) * 100.0;
                self.mem_pct.push_back((t, pct.clamp(0.0, 100.0)));
            }
            _ => {
                self.mem_uncapped = true;
                #[allow(clippy::cast_precision_loss)]
                let mb = (stats.mem_used as f64) / 1_048_576.0;
                self.mem_pct.push_back((t, mb.max(0.0)));
                if stats.mem_used > self.mem_max_bytes {
                    self.mem_max_bytes = stats.mem_used;
                }
            }
        }

        #[allow(clippy::cast_precision_loss)]
        {
            self.net_rx.push_back((t, stats.net_rx_bytes.max(0) as f64));
            self.net_tx.push_back((t, stats.net_tx_bytes.max(0) as f64));
        }

        // Trim by both time-window and absolute cap.
        let cutoff = t - HISTORY_WINDOW_SECS;
        for q in [
            &mut self.cpu_pct,
            &mut self.mem_pct,
            &mut self.net_rx,
            &mut self.net_tx,
        ] {
            while q.front().is_some_and(|(s, _)| *s < cutoff) {
                q.pop_front();
            }
            while q.len() > HISTORY_CAP {
                q.pop_front();
            }
        }
    }

    #[allow(dead_code)] // Useful for tests + future "reset on disconnect" hooks.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// True when the CPU samples ring is empty — used by the App-level
    /// GC sweep to drop history entries whose newest sample has aged
    /// out of the window (i.e. the container hasn't been seen by the
    /// poller for at least `HISTORY_WINDOW_SECS`).
    #[must_use]
    pub fn cpu_pct_is_empty(&self) -> bool {
        self.cpu_pct.is_empty()
    }

    fn x_window(&self) -> (f64, f64) {
        let now = self.started.map_or(0.0, |s| s.elapsed().as_secs_f64());
        let lo = (now - HISTORY_WINDOW_SECS).max(0.0);
        (lo, now.max(HISTORY_WINDOW_SECS))
    }

    /// Convert cumulative-bytes samples into per-second rates suitable
    /// for plotting. The first sample produces no rate; pairs of
    /// successive samples produce `(t_b, (b - a) / (t_b - t_a))`.
    fn rate_series(samples: &VecDeque<(f64, f64)>) -> Vec<(f64, f64)> {
        let mut out = Vec::with_capacity(samples.len());
        let mut prev: Option<&(f64, f64)> = None;
        for cur in samples {
            if let Some(p) = prev {
                let dt = cur.0 - p.0;
                if dt > 0.0 {
                    let rate = ((cur.1 - p.1) / dt).max(0.0);
                    out.push((cur.0, rate));
                }
            }
            prev = Some(cur);
        }
        out
    }
}

#[derive(Default)]
pub struct ContainerDetailState {
    target: Option<(Host, String)>,
    inspect: Option<ContainerDetail>,
    stats: Option<ContainerStats>,
    last_error: Option<String>,
    loaded: bool,
    /// `Some` when the operator pressed `p` and the docker-top result
    /// has landed; rendered as a modal overlay until dismissed with Esc.
    top: Option<ProcessTable>,
    /// `true` once the user has requested processes — drives the
    /// "(loading…)" modal state until the result lands.
    top_loading: bool,
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
            self.top = None;
            self.top_loading = false;
        }
        self.target = Some((host, container));
    }

    pub fn begin_top_load(&mut self) {
        self.top_loading = true;
    }

    pub fn set_top(&mut self, table: ProcessTable) {
        self.top_loading = false;
        self.top = Some(table);
    }

    pub fn dismiss_top(&mut self) {
        self.top = None;
        self.top_loading = false;
    }

    /// True when the top modal is on screen (renderer adds the overlay).
    pub fn top_visible(&self) -> bool {
        self.top_loading || self.top.is_some()
    }

    pub fn target(&self) -> Option<&(Host, String)> {
        self.target.as_ref()
    }

    pub fn apply(&mut self, data: ContainerDetailRefresh) {
        self.last_error = data.error;
        // Flip loaded unconditionally so the pane renders the error
        // path (which already exists at the top of `render`) instead
        // of looping on `(loading…)`. Last-known good inspect/stats
        // stay around to ride out transient blips. Stats history is
        // owned by the App-level shared store now (see
        // `App::container_history`); the per-render fetch only
        // refreshes the inspect block + the latest gauge sample.
        self.loaded = true;
        if self.last_error.is_none() {
            self.inspect = data.inspect;
            self.stats = data.stats;
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, history: Option<&StatsHistory>) {
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

        // Layout: header card · 5-min stats history strip · two-column
        // data block (env+labels on the right, ports/mounts/networks/
        // security on the left).
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Min(0),
            ])
            .split(area);
        Self::render_card(frame, chunks[0], inspect, self.stats.as_ref());
        Self::render_history(frame, chunks[1], history);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        Self::render_runtime(frame, cols[0], inspect);
        Self::render_env_labels(frame, cols[1], inspect);

        if self.top_visible() {
            self.render_top_modal(frame);
        }
    }

    /// Three side-by-side line charts covering the rolling 5-minute
    /// window — one each for CPU%, Mem (% when capped, MB when not),
    /// and network rx/tx rate. Splitting CPU and Mem into separate
    /// panels means each y-axis scales to its own metric: a 4 GB
    /// memory peak no longer flattens the CPU line into the bottom
    /// pixel row. The latest sampled value is rendered in each panel
    /// title so it's readable without squinting at the rightmost edge.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::too_many_lines
    )]
    fn render_history(frame: &mut Frame<'_>, area: Rect, history: Option<&StatsHistory>) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(area);

        let Some(history) = history else {
            // Empty store — render three "(collecting…)" placeholders.
            for (i, title) in ["CPU %", "Mem %", "net rx / tx"].iter().enumerate() {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} (5 min) "));
                frame.render_widget(
                    Paragraph::new("(collecting…)")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(block),
                    cols[i],
                );
            }
            return;
        };

        let (xmin, xmax) = history.x_window();
        let cpu_data: Vec<(f64, f64)> = history.cpu_pct.iter().copied().collect();
        let mem_data: Vec<(f64, f64)> = history.mem_pct.iter().copied().collect();

        // ── Panel 1: CPU% over time ───────────────────────────────────
        let cpu_now = cpu_data.last().map(|(_, v)| *v);
        let cpu_title = match cpu_now {
            Some(c) => format!(" CPU {c:>5.1}% (5 min) "),
            None => " CPU % (5 min) ".to_string(),
        };
        let cpu_block = Block::default().borders(Borders::ALL).title(cpu_title);
        if cpu_data.is_empty() {
            frame.render_widget(
                Paragraph::new("(collecting…)")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(cpu_block),
                cols[0],
            );
        } else {
            let cpu_max = cpu_data
                .iter()
                .map(|(_, v)| *v)
                .fold(0.0_f64, f64::max)
                .max(100.0);
            let datasets = vec![
                Dataset::default()
                    .name("cpu%")
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(Color::Cyan))
                    .data(&cpu_data),
            ];
            let chart = Chart::new(datasets)
                .block(cpu_block)
                .x_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([xmin, xmax])
                        .labels(time_axis_labels(xmin, xmax)),
                )
                .y_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([0.0, cpu_max])
                        .labels(percent_axis_labels(cpu_max, false)),
                );
            frame.render_widget(chart, cols[0]);
        }

        // ── Panel 2: Mem (own y-axis, % when capped, MB when uncapped) ─
        let mem_now = mem_data.last().map(|(_, v)| *v);
        let mem_unit = if history.mem_uncapped { "MB" } else { "%" };
        let mem_title = match mem_now {
            Some(m) => format!(" Mem {m:>5.1}{mem_unit} (5 min) "),
            None => format!(" Mem {mem_unit} (5 min) "),
        };
        let mem_block = Block::default().borders(Borders::ALL).title(mem_title);
        if mem_data.is_empty() {
            frame.render_widget(
                Paragraph::new("(collecting…)")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(mem_block),
                cols[1],
            );
        } else {
            let mem_max = if history.mem_uncapped {
                mem_data
                    .iter()
                    .map(|(_, v)| *v)
                    .fold(0.0_f64, f64::max)
                    .max(1.0)
            } else {
                100.0
            };
            let datasets = vec![
                Dataset::default()
                    .name(if history.mem_uncapped { "mem MB" } else { "mem%" })
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(Color::Magenta))
                    .data(&mem_data),
            ];
            let chart = Chart::new(datasets)
                .block(mem_block)
                .x_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([xmin, xmax])
                        .labels(time_axis_labels(xmin, xmax)),
                )
                .y_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([0.0, mem_max])
                        .labels(percent_axis_labels(mem_max, history.mem_uncapped)),
                );
            frame.render_widget(chart, cols[1]);
        }

        // ── Panel 3: network rx/tx rate, btop-style mirrored ──────────
        // tx (outgoing) plots above the zero line; rx (incoming) plots
        // mirrored below. Outgoing-on-top reads naturally as "this
        // container is pushing X out to the world", and the two series
        // can no longer overlap.
        let rx_rates_pos = StatsHistory::rate_series(&history.net_rx);
        let tx_rates = StatsHistory::rate_series(&history.net_tx);
        // Mirror rx below the zero line by negating each y value. The
        // rendered line still tracks the same magnitude — just on the
        // negative side of the axis.
        let rx_rates: Vec<(f64, f64)> = rx_rates_pos.iter().map(|(t, v)| (*t, -*v)).collect();
        let rx_now = rx_rates_pos.last().map_or(0.0, |(_, v)| *v);
        let tx_now = tx_rates.last().map_or(0.0, |(_, v)| *v);
        let net_title = if rx_rates.is_empty() && tx_rates.is_empty() {
            " net rx / tx (5 min) ".to_string()
        } else {
            format!(
                " net ↑{} ↓{} (5 min) ",
                format_rate(tx_now),
                format_rate(rx_now)
            )
        };
        let net_block = Block::default().borders(Borders::ALL).title(net_title);
        if rx_rates.is_empty() && tx_rates.is_empty() {
            frame.render_widget(
                Paragraph::new("(collecting…)")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(net_block),
                cols[2],
            );
        } else {
            let rx_peak = rx_rates_pos
                .iter()
                .map(|(_, v)| *v)
                .fold(0.0_f64, f64::max);
            let tx_peak = tx_rates
                .iter()
                .map(|(_, v)| *v)
                .fold(0.0_f64, f64::max);
            // Symmetric y-axis around zero so the visual zero-line
            // sits halfway down the panel regardless of whether
            // download or upload is dominant. `.max(1.0)` keeps the
            // axis non-degenerate during idle quiet periods.
            let max_rate = rx_peak.max(tx_peak).max(1.0);
            let datasets = vec![
                Dataset::default()
                    .name("↑ tx")
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(Color::Yellow))
                    .data(&tx_rates),
                Dataset::default()
                    .name("↓ rx")
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(Color::Green))
                    .data(&rx_rates),
            ];
            let chart = Chart::new(datasets)
                .block(net_block)
                .x_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([xmin, xmax])
                        .labels(time_axis_labels(xmin, xmax)),
                )
                .y_axis(
                    Axis::default()
                        .style(Style::default().fg(Color::DarkGray))
                        .bounds([-max_rate, max_rate])
                        .labels(mirrored_rate_labels(max_rate)),
                );
            frame.render_widget(chart, cols[2]);
        }
    }

    /// Centre-modal overlay listing `docker top` rows. Same dimensions
    /// as the help/error modals so the visual language stays consistent.
    fn render_top_modal(&self, frame: &mut Frame<'_>) {
        use ratatui::widgets::Clear;
        let area = frame.area();
        let modal_width = (area.width.saturating_sub(4)).clamp(60, 140);
        let modal_height = (area.height.saturating_sub(4)).clamp(8, 30);
        let x = (area.width.saturating_sub(modal_width)) / 2;
        let y = (area.height.saturating_sub(modal_height)) / 2;
        let modal_area = Rect {
            x,
            y,
            width: modal_width,
            height: modal_height,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::Cyan))
            .title(" processes (docker top — esc to close) ");
        frame.render_widget(Clear, modal_area);

        if self.top_loading && self.top.is_none() {
            let body = Paragraph::new("(loading…)")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            frame.render_widget(body, modal_area);
            return;
        }

        let Some(table) = self.top.as_ref() else {
            return;
        };
        if table.processes.is_empty() {
            let body = Paragraph::new("(no processes — container exited?)")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            frame.render_widget(body, modal_area);
            return;
        }
        // Compute per-column widths from the longest cell in each column
        // (titles + rows). Cap each column to 24 so a single huge `CMD`
        // doesn't crowd everything else off-screen.
        let n_cols = table
            .titles
            .len()
            .max(table.processes.iter().map(Vec::len).max().unwrap_or(0));
        let mut widths_chars: Vec<usize> = vec![0; n_cols];
        for (i, t) in table.titles.iter().enumerate() {
            if let Some(w) = widths_chars.get_mut(i) {
                *w = (*w).max(t.chars().count());
            }
        }
        for row in &table.processes {
            for (i, cell) in row.iter().enumerate() {
                if let Some(w) = widths_chars.get_mut(i) {
                    *w = (*w).max(cell.chars().count().min(48));
                }
            }
        }
        let widths: Vec<Constraint> = widths_chars
            .iter()
            .map(|w| Constraint::Length(u16::try_from(*w + 2).unwrap_or(u16::MAX)))
            .collect();
        let header = Row::new(
            table
                .titles
                .iter()
                .map(|t| Cell::from(t.clone()).style(bold()))
                .collect::<Vec<_>>(),
        );
        let rows: Vec<Row<'_>> = table
            .processes
            .iter()
            .map(|p| Row::new(p.iter().map(|c| Cell::from(c.clone())).collect::<Vec<_>>()))
            .collect();
        let widget = Table::new(rows, widths).header(header).block(block);
        frame.render_widget(widget, modal_area);
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

            // Cumulative network bytes since the container started.
            // Per-second rate lives in the history chart's title; this
            // line answers "how much has this thing transferred in
            // total" — useful for spotting the runaway egress case.
            right_lines.push(Line::from(vec![
                Span::styled("NET ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "↓ ".to_string(),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(format_bytes(s.net_rx_bytes)),
                Span::raw("  "),
                Span::styled(
                    "↑ ".to_string(),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format_bytes(s.net_tx_bytes)),
            ]));
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

/// Build x-axis tick labels for the rolling window. Three ticks: oldest,
/// midpoint, newest. Labels are minutes:seconds relative to "now".
fn time_axis_labels(xmin: f64, xmax: f64) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let span = (xmax - xmin).max(1.0);
    let oldest = format!("-{:.0}m", (span / 60.0).floor());
    let middle = format!("-{:.0}m", (span / 120.0).floor());
    vec![
        Span::styled(oldest, dim),
        Span::styled(middle, dim),
        Span::styled("now".to_string(), dim),
    ]
}

fn percent_axis_labels(max: f64, uncapped: bool) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    if uncapped {
        vec![
            Span::styled("0".to_string(), dim),
            Span::styled(format!("{:.0}M", max / 2.0), dim),
            Span::styled(format!("{max:.0}M"), dim),
        ]
    } else {
        vec![
            Span::styled("0%".to_string(), dim),
            Span::styled(format!("{:.0}%", max / 2.0), dim),
            Span::styled(format!("{max:.0}%"), dim),
        ]
    }
}

/// Labels for the mirrored rx/tx panel: bottom is `↓ rx_max` (green —
/// incoming, plotted below zero), middle is `0`, top is `↑ tx_max`
/// (yellow — outgoing, plotted above zero). Both magnitudes are
/// positive (the y values are signed but the operator reads the
/// magnitude with the arrow indicating direction).
fn mirrored_rate_labels(max: f64) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let up = Style::default().fg(Color::Yellow);
    let down = Style::default().fg(Color::Green);
    vec![
        Span::styled(format!("↓{}", format_rate(max)), down),
        Span::styled("0".to_string(), dim),
        Span::styled(format!("↑{}", format_rate(max)), up),
    ]
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1024.0 {
        format!("{bytes_per_sec:.0}B/s")
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.1}KB/s", bytes_per_sec / 1024.0)
    } else if bytes_per_sec < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else {
        format!("{:.2}GB/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Background-friendly `docker top` fetch — runs off the event loop
/// so a slow daemon doesn't freeze the TUI.
pub async fn fetch_top(
    ops: Arc<dyn DockerOps>,
    host: Host,
    container: String,
) -> Result<ProcessTable, String> {
    ops.top_container(&host, &container)
        .await
        .map_err(|e| e.to_string())
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
