//! Drift inspection modal — answers "what about this service drifted?"
//! without leaving the TUI.
//!
//! The data shown is the same `FieldDiff` `yoink up --plan` prints
//! (image, tag, `spec_hash`, env keys, label keys). The modal opens
//! on `~` from any view that has a focused (host, service) — list
//! views resolve the focused row, the container detail view derives
//! it from the inspected container.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::diff::{ChangeKind, FieldDiff, ServiceDiff};
use crate::docker_ops::Host;

/// Outcome of a drift fetch — either the matching `ServiceDiff` for
/// the focused (host, service) pair, or `None` when the host yielded
/// no row (rare: a service mid-removal). Errors flow back as `Err`.
pub type DriftRefresh = Result<Option<ServiceDiff>, String>;

#[derive(Default)]
pub struct DriftState {
    target: Option<(Host, String)>,
    loading: bool,
    diff: Option<ServiceDiff>,
    error: Option<String>,
}

impl DriftState {
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.target.is_some()
    }

    /// Begin a fetch for `(host, service)`. Subsequent `apply` for a
    /// different target is dropped — the user re-pressed `~` on a
    /// different row before the previous fetch landed.
    pub fn set_target(&mut self, host: Host, service: String) {
        self.target = Some((host, service));
        self.loading = true;
        self.diff = None;
        self.error = None;
    }

    pub fn apply(&mut self, host: &Host, service: &str, result: DriftRefresh) {
        if self
            .target
            .as_ref()
            .is_none_or(|(h, s)| h != host || s != service)
        {
            return;
        }
        self.loading = false;
        match result {
            Ok(Some(diff)) => self.diff = Some(diff),
            Ok(None) => self.error = Some("no drift data for this host".into()),
            Err(e) => self.error = Some(e),
        }
    }

    pub fn clear(&mut self) {
        self.target = None;
        self.loading = false;
        self.diff = None;
        self.error = None;
    }
}

/// Center-of-screen modal mirroring the help/log modal sizing.
/// Header strip carries `Esc` close hint; body shows the diff with
/// `+`/`-`/`~` color coding (green/red/yellow) — same legend the
/// CLI's terraform-style text format uses.
pub fn render_modal(
    frame: &mut Frame<'_>,
    state: &DriftState,
    throbber: &throbber_widgets_tui::ThrobberState,
) {
    let Some((host, service)) = state.target.as_ref() else {
        return;
    };
    let area = frame.area();
    let modal_width = (area.width.saturating_sub(4)).clamp(50, 100);
    let modal_height = (area.height.saturating_sub(4)).clamp(10, 30);
    let x = (area.width.saturating_sub(modal_width)) / 2;
    let y = (area.height.saturating_sub(modal_height)) / 2;
    let rect = Rect {
        x,
        y,
        width: modal_width,
        height: modal_height,
    };

    let title = format!(" drift: {service} on {} (esc to close) ", host.address);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);

    let lines = if state.loading {
        vec![super::ui::loading_line(throbber)]
    } else if let Some(err) = state.error.as_deref() {
        vec![
            Line::from(Span::styled(
                format!("✗ {err}"),
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "(running `yoink up --plan --service <name>` from the CLI",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "may surface a richer error message)",
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else if let Some(diff) = state.diff.as_ref() {
        body_lines(diff)
    } else {
        vec![Line::from("(no data)")]
    };

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block),
        rect,
    );
}

fn body_lines(diff: &ServiceDiff) -> Vec<Line<'static>> {
    match &diff.change {
        ChangeKind::NoOp { current_hash } => vec![Line::from(vec![
            Span::styled(
                "= no drift",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" (spec_hash {})", short(current_hash))),
        ])],
        ChangeKind::Create { desired_image } => vec![
            Line::from(vec![
                Span::styled(
                    "+ would create",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {desired_image}")),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "no running replica on this host yet — nothing to diff against.",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        ChangeKind::Update {
            current_hash,
            current_image,
            desired_image,
            fields,
        } => update_body(current_hash, current_image, desired_image, &diff.tag, &diff.desired_hash, fields),
    }
}

fn update_body(
    current_hash: &str,
    current_image: &str,
    desired_image: &str,
    desired_tag: &str,
    desired_hash: &str,
    fields: &FieldDiff,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(kv_diff_line("image", current_image, desired_image));
    out.push(kv_diff_line("spec", &short(current_hash), &short(desired_hash)));
    out.push(Line::from(vec![
        Span::styled("    tag  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            desired_tag.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ]));
    if !fields.is_empty() {
        out.push(Line::from(""));
        push_kv_group(&mut out, "env", &fields.env_added, &fields.env_removed, &fields.env_changed);
        push_kv_group(
            &mut out,
            "labels",
            &fields.labels_added,
            &fields.labels_removed,
            &fields.labels_changed,
        );
    }
    out
}

fn kv_diff_line(label: &str, current: &str, desired: &str) -> Line<'static> {
    if current == desired {
        Line::from(vec![
            Span::styled(format!("{label:>8}  "), Style::default().fg(Color::DarkGray)),
            Span::raw(current.to_string()),
            Span::styled(
                "  (unchanged)".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("{label:>8}  "), Style::default().fg(Color::DarkGray)),
            Span::styled(current.to_string(), Style::default().fg(Color::Yellow)),
            Span::styled(" → ".to_string(), Style::default().fg(Color::DarkGray)),
            Span::styled(
                desired.to_string(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ])
    }
}

fn push_kv_group(
    out: &mut Vec<Line<'static>>,
    label: &str,
    added: &[String],
    removed: &[String],
    changed: &[String],
) {
    if added.is_empty() && removed.is_empty() && changed.is_empty() {
        return;
    }
    out.push(Line::from(Span::styled(
        format!("{label}:"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    for k in added {
        out.push(prefixed("  + ", k, Color::Green));
    }
    for k in removed {
        out.push(prefixed("  - ", k, Color::Red));
    }
    for k in changed {
        out.push(prefixed("  ~ ", k, Color::Yellow));
    }
}

fn prefixed(marker: &str, key: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            marker.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(key.to_string()),
    ])
}

fn short(h: &str) -> String {
    h[..h.len().min(7)].to_string()
}
