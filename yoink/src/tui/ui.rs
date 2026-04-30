//! Style + layout helpers shared by all panes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, TableState};
use throbber_widgets_tui::{Throbber, ThrobberState};

use crate::config::Config;
use crate::deploy;
use crate::docker;
use crate::docker_ops::ContainerInfo;
use crate::secrets::SecretsBundle;

/// Bold style for table headers and pane titles.
#[must_use]
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn throbber_glyph_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Animated "loading…" line, shared by every pane that drops a
/// placeholder row while a docker fetch is in flight. Drives off the
/// `throbber-widgets-tui` crate so the spinner glyph rotates with the
/// app's `ThrobberState` (advanced once per fast UI tick) — clearly
/// distinguishing "still working" from "stuck on an old result".
#[must_use]
pub fn loading_line(state: &ThrobberState) -> Line<'static> {
    throbber_with_label(state, " loading…")
}

/// Same as [`loading_line`] but with a caller-supplied label.
/// Use it when "loading" is the wrong word (e.g. " running…",
/// " pulling…") so the spinner reads naturally for the action
/// it's animating.
#[must_use]
pub fn throbber_with_label(state: &ThrobberState, label: &'static str) -> Line<'static> {
    Throbber::default()
        .label(label)
        .style(Style::default().fg(Color::DarkGray))
        .throbber_style(throbber_glyph_style())
        .to_line(state)
}

/// Just the rotating throbber glyph — useful when you need to embed
/// the spinner inside a richer line (a modal title, a status table
/// row) where the surrounding spans already carry their own styling.
#[must_use]
pub fn throbber_span(state: &ThrobberState) -> Span<'static> {
    Throbber::default()
        .throbber_style(throbber_glyph_style())
        .to_symbol_span(state)
}

/// Compose a modal title `Line`. When `throbber` is `Some` (job is
/// still in flight) a rotating glyph is prepended so the operator
/// has a visible "still working" cue independent of the streaming
/// log — a streaming log that has stalled for 30 seconds looks the
/// same as a finished one until the spinner stops moving.
#[must_use]
fn modal_title(title: &str, throbber: Option<&ThrobberState>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" "));
    if let Some(state) = throbber {
        spans.push(throbber_span(state));
    }
    spans.push(Span::raw(title.to_string()));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Drift cell shared by every list view (dashboard, host detail,
/// service detail). ✓ in-sync (green), ⚠ drift (yellow), ? unknown
/// (dim — container has no `yoink.spec_hash`, isn't yoink-managed,
/// secrets bundle hasn't loaded, or the desired-spec build failed).
#[must_use]
pub fn render_drift_cell(
    container: &ContainerInfo,
    config: &Config,
    secrets: Option<&SecretsBundle>,
) -> Cell<'static> {
    let unknown = || Cell::from("?").style(Style::default().fg(Color::DarkGray));

    let Some(service_name) = container.yoink_service.as_deref() else {
        return unknown();
    };
    let Some(running_hash) = container.yoink_spec_hash.as_deref() else {
        return unknown();
    };
    let Some(service_cfg) = config.services.iter().find(|s| s.name == service_name) else {
        return unknown();
    };
    // For services with a config-pinned tag, use it. Otherwise fall
    // back to the running container's tag — measures config-spec-only
    // drift (env / network / options / mounts) instead of pretending
    // we know what tag would be deployed.
    let tag = service_cfg
        .tag
        .clone()
        .or_else(|| container.yoink_version.clone());
    let Some(tag) = tag else {
        return unknown();
    };

    let Ok(desired) = deploy::build_desired_spec(config, service_cfg, &tag, secrets) else {
        return unknown();
    };
    let desired_hash = docker::compute_spec_hash(&desired);

    let has_secrets = !service_cfg.secrets.is_empty() || !service_cfg.env_from_secrets.is_empty();
    if has_secrets && secrets.is_none() {
        return unknown();
    }

    if desired_hash == running_hash {
        Cell::from("✓ sync").style(Style::default().fg(Color::Green))
    } else {
        Cell::from("⚠ drift").style(Style::default().fg(Color::Yellow))
    }
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

/// Color for the "state" column based on docker's container state.
/// Lifecycle (running → exited / dead) maps onto the obvious colors;
/// transient states get yellow so they stand out at a glance.
#[must_use]
pub fn state_style(state: &str) -> Style {
    match state.to_ascii_lowercase().as_str() {
        "running" => Style::default().fg(Color::Green),
        "exited" | "dead" => Style::default().fg(Color::DarkGray),
        "restarting" | "removing" | "paused" => Style::default().fg(Color::Yellow),
        "created" => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}

/// Compact a docker image reference for table-cell display: drop
/// the registry prefix (everything up to the last `/`) and truncate
/// `sha256:…` digests so the cell stays readable.
/// `ghcr.io/you/api:abcd1234` becomes `api:abcd1234`.
#[must_use]
pub fn short_image(image: &str) -> String {
    if image.is_empty() {
        return "-".into();
    }
    if let Some(rest) = image.strip_prefix("sha256:") {
        let head: String = rest.chars().take(12).collect();
        return format!("sha256:{head}");
    }
    let (path, tag) = image
        .split_once('@')
        .unwrap_or_else(|| image.rsplit_once(':').map_or((image, ""), |(p, t)| (p, t)));
    let last = path.rsplit('/').next().unwrap_or(path);
    if tag.is_empty() {
        last.to_string()
    } else if tag.starts_with("sha256:") {
        let head: String = tag.chars().take(19).collect();
        format!("{last}@{head}…")
    } else {
        format!("{last}:{tag}")
    }
}

/// Right-aligned dim key + bold value on one line — the shape every
/// detail card uses ("image", "state", "command", …).
#[must_use]
pub fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>10}  "), Style::default().fg(Color::DarkGray)),
        Span::styled(
            value.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Variant of `kv` where the value carries a caller-supplied style
/// (e.g. state-colored, health-colored) — bold modifier is layered
/// on top so the cell still reads as a value.
#[must_use]
pub fn kv_styled(key: &str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>10}  "), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), value_style.add_modifier(Modifier::BOLD)),
    ])
}

/// Carve the global header off the top of `area`. The header is a
/// 4-row bordered block hosting the tab bar (row 1) + breadcrumb
/// (row 2). Returns (`header_area`, `pane_area`).
#[must_use]
pub fn split_with_header(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);
    (chunks[0], chunks[1])
}

/// Render the global header: a bordered cyan block containing a
/// `Tabs` row over a breadcrumb row. `tabs` is the ordered list of
/// top-level section labels; `selected_tab` is the currently active
/// index (or `None` to dim the whole bar — used inside drill-down
/// views that don't map cleanly to a single top-level tab).
///
/// `slug` is rendered centered in the top border in bold red — a
/// loud, always-visible reminder that the operator opted into
/// (e.g. "PRODUCTION — TREAD CAREFULLY") for configs where typing
/// `up` should give pause. `source` shows where the config came from
/// (typically "<git-repo>" or "<git-repo>(*)" when dirty), aligned
/// right in the top border.
#[allow(clippy::too_many_arguments)]
pub fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    tabs: &[&str],
    selected_tab: Option<usize>,
    crumbs: &[String],
    right: &str,
    slug: Option<&str>,
    source: Option<&str>,
) {
    use ratatui::widgets::Tabs;

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray))
        .title(Line::from(Span::styled(
            " yoink ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    if let Some(text) = slug.map(str::trim).filter(|s| !s.is_empty()) {
        block = block.title(
            Line::from(Span::styled(
                format!(" {text} "),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }
    if let Some(text) = source.map(str::trim).filter(|s| !s.is_empty()) {
        block = block.title(
            Line::from(Span::styled(
                format!(" {text} "),
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    // Inactive tabs at Color::White stay readable on dark and light
    // terminals; the active tab pops via reverse-video cyan.
    let tab_titles: Vec<Line<'_>> = tabs.iter().map(|t| Line::from(*t)).collect();
    let tabs_widget = Tabs::new(tab_titles)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .divider(Span::styled(" │ ", Style::default().fg(Color::Gray)))
        .select(selected_tab.unwrap_or(usize::MAX));
    frame.render_widget(tabs_widget, inner_chunks[0]);

    render_breadcrumb_line(frame, inner_chunks[1], crumbs, right);
}

fn render_breadcrumb_line(frame: &mut Frame<'_>, area: Rect, crumbs: &[String], right: &str) {
    // Color::Gray (bright on dark terminals, dark on light) reads
    // cleanly in both directions where DarkGray fades into the bg.
    let dim = Style::default().fg(Color::Gray);
    let leaf = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let last = crumbs.len().saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(crumbs.len() * 2);
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" › ".to_string(), dim));
        }
        let style = if i == last { leaf } else { dim };
        spans.push(Span::styled(c.clone(), style));
    }
    let left = Paragraph::new(Line::from(spans));
    frame.render_widget(left, area);

    if !right.is_empty() {
        let right_width = u16::try_from(right.chars().count()).unwrap_or(u16::MAX);
        if area.width > right_width {
            let right_area = Rect {
                x: area.x + area.width - right_width,
                y: area.y,
                width: right_width,
                height: 1,
            };
            let right_para = Paragraph::new(Line::from(Span::styled(right.to_string(), dim)));
            frame.render_widget(right_para, right_area);
        }
    }
}

/// Style applied to the highlighted row in selectable tables
/// (`Hosts`, `HostDetail`, `Services`, `ServiceDetail`). Reverse-video
/// on cyan reads as "this is the cursor" without fighting any of the
/// per-cell colors (state/health/etc.) underneath.
#[must_use]
pub fn table_highlight_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// `▶ ` prefix on the highlighted row — pairs with
/// `table_highlight_style`. Used by every selectable table.
pub const TABLE_HIGHLIGHT_SYMBOL: &str = "▶ ";

/// Copy `text` to the host terminal's system clipboard via the
/// OSC-52 escape sequence — works in iTerm2, Kitty, Alacritty,
/// `WezTerm`, Ghostty, modern Terminal.app, and tmux (with
/// `set -g set-clipboard on`). No external `pbcopy` / `xclip` dep,
/// and crucially works over SSH if the local terminal honors
/// OSC-52. Returns the byte count copied (caller toasts it).
pub fn copy_to_clipboard(text: &str) -> std::io::Result<usize> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::io::Write;
    let encoded = STANDARD.encode(text.as_bytes());
    let payload = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = std::io::stdout();
    stdout.write_all(payload.as_bytes())?;
    stdout.flush()?;
    Ok(text.len())
}

/// Green / yellow / red threshold for a 0..=1 gauge — the convention
/// matches what the real `Gauge` widget uses by default. <60% green,
/// <85% yellow, otherwise red.
#[must_use]
pub fn gauge_color(ratio: f32) -> Color {
    if ratio < 0.60 {
        Color::Green
    } else if ratio < 0.85 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Inline mini-gauge for a table cell, drawn with the same eighth-blocks
/// the real `Gauge` widget uses. Returns three styled spans: opening
/// bracket, the bar (colored fill + dim unfilled), closing bracket.
/// The brackets make the bounds obvious — without them the bar floats
/// in whitespace and you can't tell where 0% / 100% sit.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn inline_gauge(ratio: f32, width: usize, color: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let bracket_style = Style::default().fg(Color::DarkGray);
    let r = ratio.clamp(0.0, 1.0);
    let total_eighths = (r * (width as f32) * 8.0).round() as usize;
    let full = total_eighths / 8;
    let partial_idx = total_eighths % 8;
    let partials = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

    let mut filled = String::with_capacity(width);
    for _ in 0..full.min(width) {
        filled.push('█');
    }
    if full < width && partial_idx > 0 {
        filled.push(partials[partial_idx]);
    }
    // Pad the remaining slots with a faint dotted glyph so the
    // unfilled portion is visible — pure spaces look like missing
    // data on most terminals.
    let used = filled.chars().count();
    let mut empty = String::with_capacity(width.saturating_sub(used));
    for _ in 0..width.saturating_sub(used) {
        empty.push('·');
    }

    vec![
        Span::styled("[".to_string(), bracket_style),
        Span::styled(filled, Style::default().fg(color)),
        Span::styled(empty, bracket_style),
        Span::styled("]".to_string(), bracket_style),
    ]
}

/// Substring-filter state shared by every list/table pane.
///
/// Filters live incrementally (vim-`/` style): every keystroke
/// updates `filter` immediately, so the visible rows shrink as the
/// operator types. `Enter` commits the filter and exits input mode;
/// `Esc` cancels and restores whatever was active before `/` was
/// pressed. `matches(text)` returns true when there's no filter, or
/// when `text` (case-insensitive) contains the active filter.
#[derive(Default, Debug, Clone)]
pub struct FilterState {
    /// Currently-applied filter; rows whose searched text doesn't
    /// contain this (case-insensitive) are hidden. Updated on every
    /// keystroke while in input mode so the table re-filters live.
    filter: Option<String>,
    /// Lowercased copy of `filter` — cached so per-row `matches()`
    /// during a render doesn't have to lowercase the (constant)
    /// filter every time. Kept in lockstep with `filter`.
    filter_lc: Option<String>,
    /// `Some(buf)` while the user is typing a new filter via `/`.
    /// Mirrors `filter` while typing — split out so the footer can
    /// render the literal buffer (with cursor) regardless of the
    /// applied state.
    input_buffer: Option<String>,
    /// Snapshot of `filter` taken when `/` was pressed — restored on
    /// `cancel` so Esc abandons the in-progress search and reverts to
    /// the prior view.
    prev_filter: Option<String>,
}

impl FilterState {
    pub fn input_mode(&self) -> bool {
        self.input_buffer.is_some()
    }

    pub fn begin_input(&mut self) {
        self.prev_filter = self.filter.clone();
        self.input_buffer = Some(self.filter.clone().unwrap_or_default());
    }

    pub fn push_char(&mut self, c: char) {
        if let Some(buf) = self.input_buffer.as_mut() {
            buf.push(c);
            self.sync_filter_from_buffer();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(buf) = self.input_buffer.as_mut() {
            buf.pop();
            self.sync_filter_from_buffer();
        }
    }

    /// Commit the typed filter — exit input mode; `filter` is already
    /// up to date (each keystroke synced it). Drops the prev-filter
    /// snapshot so a subsequent Esc on a fresh `/` doesn't restore
    /// stale state.
    pub fn apply(&mut self) {
        self.input_buffer = None;
        self.prev_filter = None;
    }

    /// Abandon the in-progress search — restore whatever filter was
    /// active before `/` was pressed.
    pub fn cancel(&mut self) {
        self.input_buffer = None;
        let prev = self.prev_filter.take();
        self.set_filter(prev);
    }

    pub fn clear(&mut self) {
        self.filter = None;
        self.filter_lc = None;
        self.input_buffer = None;
        self.prev_filter = None;
    }

    /// Re-derive `filter` + `filter_lc` from the current input buffer.
    /// Empty buffer means "no filter" so the user typing `/` then
    /// backspacing back to empty restores the unfiltered view.
    fn sync_filter_from_buffer(&mut self) {
        let buf = self.input_buffer.clone().unwrap_or_default();
        if buf.is_empty() {
            self.filter = None;
            self.filter_lc = None;
        } else {
            self.filter_lc = Some(buf.to_ascii_lowercase());
            self.filter = Some(buf);
        }
    }

    fn set_filter(&mut self, value: Option<String>) {
        match value {
            None => {
                self.filter = None;
                self.filter_lc = None;
            }
            Some(s) => {
                self.filter_lc = Some(s.to_ascii_lowercase());
                self.filter = Some(s);
            }
        }
    }

    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    #[must_use]
    pub fn input_buffer(&self) -> Option<&str> {
        self.input_buffer.as_deref()
    }

    /// Case-insensitive substring match. Returns true when there's no
    /// active filter (so unfiltered rows still render).
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        match &self.filter_lc {
            None => true,
            Some(f) => text.to_ascii_lowercase().contains(f.as_str()),
        }
    }
}

/// Footer text for a pane with a filter — yellow editable line while
/// typing, dim help line otherwise. Caller passes the help text it'd
/// have shown without a filter.
#[must_use]
pub fn filter_footer(filter: &FilterState, default_help: &str) -> Paragraph<'static> {
    if let Some(buf) = filter.input_buffer() {
        Paragraph::new(format!("/{buf}_  (live · enter keep · esc revert)"))
            .style(Style::default().fg(Color::Yellow))
    } else if let Some(f) = filter.current() {
        Paragraph::new(format!(
            "filter: {f}  ·  / edit · esc clear · {default_help}"
        ))
        .style(Style::default().fg(Color::Cyan))
    } else {
        Paragraph::new(format!("/ filter · {default_help}"))
            .style(Style::default().fg(Color::DarkGray))
    }
}

/// Render a vertical scrollbar overlay on the right edge of `area`.
/// `position` is the index of the topmost visible item; `total` is
/// the number of items in the underlying buffer; `viewport` is how
/// many rows fit on screen. No-op if everything fits.
pub fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    position: usize,
    total: usize,
    viewport: usize,
) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    if total <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(total.saturating_sub(viewport)).position(position);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(bar, area, &mut state);
}

/// Big centered modal that displays a streaming log, auto-scrolled
/// so the newest line is always visible. `success` / `failure` tint
/// the border (green / red) once the producing task is done — both
/// false means "still running". When `throbber` is `Some` the title
/// gets a leading spinner glyph so the operator can distinguish
/// "still working" from "frozen".
pub fn render_log_modal(
    frame: &mut Frame<'_>,
    title: &str,
    lines: &[String],
    success: bool,
    failure: bool,
    throbber: Option<&ThrobberState>,
) {
    let area = frame.area();
    // 90% of the available area, capped at 120×40 so the modal feels
    // like a focused panel rather than the whole screen.
    let modal_width = (area.width.saturating_sub(4)).clamp(40, 120);
    let modal_height = (area.height.saturating_sub(4)).clamp(10, 40);
    let x = (area.width.saturating_sub(modal_width)) / 2;
    let y = (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_width,
        height: modal_height,
    };

    let border_color = if success {
        Color::Green
    } else if failure {
        Color::Red
    } else {
        Color::Cyan
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(modal_title(title, throbber));

    // Auto-scroll: keep the newest line glued to the bottom.
    let inner_height = modal_area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let skip = total.saturating_sub(inner_height);
    let visible: Vec<Line<'static>> = lines
        .iter()
        .skip(skip)
        .map(|l| Line::from(l.clone()))
        .collect();

    frame.render_widget(Clear, modal_area);
    frame.render_widget(Paragraph::new(visible).block(block), modal_area);
}

/// Same shape as `render_log_modal` but with a per-service status
/// table at the top (one row per service, colored by state) and the
/// scrolling event log below. Used for `JobKind::ReconcileAll` so
/// concurrent waves are legible at a glance — the status table is the
/// "where is everyone right now?" overview, the log is the detail.
#[allow(clippy::too_many_arguments)]
pub fn render_status_log_modal(
    frame: &mut Frame<'_>,
    title: &str,
    statuses: &[(String, String, Color)],
    lines: &[String],
    success: bool,
    failure: bool,
    throbber: Option<&ThrobberState>,
) {
    let area = frame.area();
    let modal_width = (area.width.saturating_sub(4)).clamp(40, 120);
    let modal_height = (area.height.saturating_sub(4)).clamp(10, 40);
    let x = (area.width.saturating_sub(modal_width)) / 2;
    let y = (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_width,
        height: modal_height,
    };

    let border_color = if success {
        Color::Green
    } else if failure {
        Color::Red
    } else {
        Color::Cyan
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(modal_title(title, throbber));
    frame.render_widget(Clear, modal_area);
    frame.render_widget(&outer, modal_area);

    // Inside the outer block, split vertically: top = status table
    // (height = N services + 2 for borders, capped), bottom = log.
    let inner = outer.inner(modal_area);
    let table_height = u16::try_from(statuses.len())
        .unwrap_or(0)
        .saturating_add(2)
        .min(inner.height.saturating_sub(3));
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(table_height), Constraint::Min(3)])
        .split(inner);

    // Status table — name padded to align the labels.
    let name_width = statuses.iter().map(|(n, _, _)| n.len()).max().unwrap_or(8);
    let status_lines: Vec<Line<'static>> = statuses
        .iter()
        .map(|(name, label, color)| {
            Line::from(vec![
                Span::raw(format!(" {name:<name_width$}  ")),
                Span::styled(label.clone(), Style::default().fg(*color)),
            ])
        })
        .collect();
    let status_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(status_lines).block(status_block), split[0]);

    // Scrolling event log — auto-scroll, newest at bottom.
    let log_inner_height = split[1].height as usize;
    let total = lines.len();
    let skip = total.saturating_sub(log_inner_height);
    let visible: Vec<Line<'static>> = lines
        .iter()
        .skip(skip)
        .map(|l| Line::from(l.clone()))
        .collect();
    frame.render_widget(Paragraph::new(visible), split[1]);
}

/// Render a centered modal overlay with `lines` of text, sized to fit
/// the longest line. Used for the `?` help overlay; could host other
/// modal dialogs later. Caller is responsible for matching key
/// handling (Esc / `?` to dismiss).
pub fn render_modal(frame: &mut Frame<'_>, title: &str, lines: &[&str]) {
    let area = frame.area();
    let max_width = lines
        .iter()
        .map(|l| u16::try_from(l.chars().count()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(40)
        .saturating_add(4);
    let modal_width = max_width.min(area.width.saturating_sub(4));
    let modal_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(modal_width)) / 2;
    let y = (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect {
        x,
        y,
        width: modal_width,
        height: modal_height,
    };
    let body = lines.iter().copied().map(Line::from).collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(Clear, modal_area);
    frame.render_widget(Paragraph::new(body).block(block), modal_area);
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
