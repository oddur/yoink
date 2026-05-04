//! Logs pane. Lines are pushed by tokio tasks running per-container log
//! streams from `DockerOps::open_log_stream`. The buffer is bounded so
//! it can't grow without limit. Supports vertical scrolling and a
//! substring filter.
//!
//! Default behavior is "follow" — newly arriving lines auto-scroll into
//! view. Any explicit upward scroll switches off follow; pressing `G` or
//! `End` re-pins to the bottom and re-enables follow.

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::config::Config;

use super::ui::{FilterState, bold, pane_layout};

const LINE_LIMIT: usize = 5_000;

/// Log-level filter passed to `hl --level <LEVEL>` (level >= floor).
/// Mirrors hl's accepted values exactly so we don't need to translate.
/// `All` is the no-op default (no `--level` flag passed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    #[default]
    All,
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    /// Cycle in increasing-strictness order:
    /// `All → Trace → Debug → Info → Warning → Error → All`. Pressing
    /// the level key repeatedly walks through the same set the user
    /// would write on the CLI, ending back at `All` so the binding is
    /// reversible without a separate "decrease" key.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Trace,
            Self::Trace => Self::Debug,
            Self::Debug => Self::Info,
            Self::Info => Self::Warning,
            Self::Warning => Self::Error,
            Self::Error => Self::All,
        }
    }

    /// Value to pass to `hl --level=<X>`. `None` means "don't pass
    /// the flag" — hl's default shows everything.
    #[must_use]
    pub fn hl_arg(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Trace => Some("trace"),
            Self::Debug => Some("debug"),
            Self::Info => Some("info"),
            Self::Warning => Some("warning"),
            Self::Error => Some("error"),
        }
    }

    /// Compact label for the header: `all|trace|debug|info|warn|error`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Default)]
pub struct LogsState {
    lines: Vec<RenderedLine>,
    /// Vertical line offset (lines from top of filtered view) when
    /// `auto_follow` is false. Render clamps to `[0, max_scroll]`.
    scroll: u16,
    /// When true, scroll snaps to the bottom on each render so newest
    /// lines stay in view.
    auto_follow: bool,
    /// Shared incremental-filter state — lines whose plain text
    /// doesn't contain the active substring are hidden.
    filter: FilterState,
    /// Toggled by 'w'. Wraps long lines instead of truncating.
    wrap: bool,
    /// Cycled by 'L'. Drives the `--level=<X>` flag yoink passes to
    /// the per-container `hl` children. Stored on the state so the
    /// label is rendered alongside the filter status.
    level: LogLevel,
}

/// One line in the buffer. `plain` is what the filter matches against;
/// `styled` is what we actually render — when piped through `hl`, it
/// carries colored ANSI spans converted to ratatui style.
pub struct RenderedLine {
    pub plain: String,
    pub styled: Line<'static>,
}

impl RenderedLine {
    /// Default formatting for a raw log line: `"[container] [out|err] message"`,
    /// no styling. Used by both the non-`hl` forwarder and the test-only
    /// `LogsState::push_line` helper so both paths agree.
    #[must_use]
    pub fn from_log_line(line: &crate::docker_ops::LogLine) -> Self {
        use crate::docker_ops::LogStream;
        let kind = match line.stream {
            LogStream::Stdout => "out",
            LogStream::Stderr => "err",
        };
        let plain = format!("[{}] [{}] {}", line.container, kind, line.message);
        Self {
            styled: Line::from(plain.clone()),
            plain,
        }
    }
}

impl LogsState {
    pub fn new() -> Self {
        Self {
            auto_follow: true,
            ..Self::default()
        }
    }

    /// Test-only convenience: push a raw `LogLine` formatted via
    /// `RenderedLine::from_log_line`.
    #[cfg(test)]
    pub fn push_line(&mut self, line: &crate::docker_ops::LogLine) {
        self.push_rendered(RenderedLine::from_log_line(line));
    }

    /// Push a pre-formatted line (e.g. one that came back from `hl`).
    pub fn push_rendered(&mut self, line: RenderedLine) {
        self.lines.push(line);
        if self.lines.len() > LINE_LIMIT {
            let drop = self.lines.len() - LINE_LIMIT;
            self.lines.drain(0..drop);
        }
    }

    /// Plain-text snapshot of the currently-visible (filtered) lines,
    /// joined by newline — what the `y` key copies to the system
    /// clipboard.
    #[must_use]
    pub fn copy_text(&self) -> String {
        let mut out = String::new();
        for line in self.filter_iter() {
            out.push_str(&line.plain);
            out.push('\n');
        }
        out
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll = 0;
        self.auto_follow = true;
    }

    // ─── scroll ────────────────────────────────────────────────────────

    pub fn scroll_up(&mut self, n: u16) {
        self.auto_follow = false;
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.auto_follow = false;
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn jump_to_top(&mut self) {
        self.auto_follow = false;
        self.scroll = 0;
    }

    pub fn jump_to_bottom(&mut self) {
        self.auto_follow = true;
        self.scroll = 0;
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }

    /// Step the level filter forward and clear the buffer — the
    /// existing lines were rendered by the previous `hl` instances
    /// without the new filter, so keeping them around would be
    /// misleading. Caller is responsible for restarting the streams
    /// so each container's `hl` child gets re-spawned with the new
    /// `--level` arg.
    pub fn cycle_level(&mut self) {
        self.level = self.level.next();
        self.clear();
    }

    #[must_use]
    pub fn level(&self) -> LogLevel {
        self.level
    }

    // ─── filter input ──────────────────────────────────────────────────

    pub fn input_mode(&self) -> bool {
        self.filter.input_mode()
    }

    pub fn begin_filter_input(&mut self) {
        self.filter.begin_input();
    }

    pub fn filter_push_char(&mut self, c: char) {
        self.filter.push_char(c);
    }

    pub fn filter_backspace(&mut self) {
        self.filter.backspace();
    }

    /// Commit the filter — exit input mode and re-pin to the bottom
    /// so the newest matching lines are visible.
    pub fn filter_apply(&mut self) {
        self.filter.apply();
        self.auto_follow = true;
        self.scroll = 0;
    }

    /// Abandon the in-progress filter — restore whatever filter was
    /// active before `/` was pressed.
    pub fn filter_cancel(&mut self) {
        self.filter.cancel();
    }

    #[cfg(test)]
    pub fn current_filter(&self) -> Option<&str> {
        self.filter.current()
    }

    // ─── rendering ─────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, config: &Config) {
        let layout = pane_layout(area);

        // Filter once, then render. We collect Line clones because the
        // Paragraph widget wants owned Text<'static>.
        let filtered: Vec<Line<'static>> = self.filter_iter().map(|l| l.styled.clone()).collect();
        let total = u16::try_from(filtered.len()).unwrap_or(u16::MAX);

        // Body height inside the bordered block = inner area height.
        let inner_height = layout[1].height.saturating_sub(2);
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = if self.auto_follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        // Persist clamped value so jumping past end doesn't keep accumulating.
        self.scroll = scroll;

        let services = config
            .services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let header_text = format!(
            "yoink logs · services: {services} · {} lines · level: {}{} · {}",
            total,
            self.level.label(),
            self.filter
                .current()
                .map(|f| format!(" · filter: {f}"))
                .unwrap_or_default(),
            if self.auto_follow { "follow" } else { "paused" },
        );
        let header = Paragraph::new(header_text).style(bold());
        frame.render_widget(header, layout[0]);

        let body: Paragraph<'_> = if filtered.is_empty() {
            let placeholder = if self.lines.is_empty() {
                "(no log lines yet — waiting for streams)"
            } else {
                "(no lines match the current filter)"
            };
            Paragraph::new(placeholder).style(Style::default().fg(Color::DarkGray))
        } else {
            let p = Paragraph::new(Text::from(filtered)).scroll((scroll, 0));
            // `trim: false` preserves indentation on wrapped
            // continuation lines (stack traces, structured-log
            // multi-line values).
            if self.wrap {
                p.wrap(Wrap { trim: false })
            } else {
                p
            }
        }
        .block(Block::default().borders(Borders::ALL).title("logs"));
        frame.render_widget(body, layout[1]);

        // Vertical scrollbar overlaid on the right edge — gives the
        // operator a sense of "where in the buffer am I" while
        // tailing or scrolling back through history. No-op when the
        // buffer fits in the viewport.
        super::ui::render_vertical_scrollbar(
            frame,
            layout[1],
            usize::from(scroll),
            usize::from(total),
            usize::from(inner_height),
        );

        let wrap_hint = if self.wrap { "w wrap*" } else { "w wrap" };
        let default_help = format!(
            "q quit · k clear · / filter · L level ({}) · ↑↓ scroll · g top · G bottom · {wrap_hint} · d dashboard · h hosts",
            self.level.label(),
        );
        frame.render_widget(
            super::ui::filter_footer(&self.filter, &default_help),
            layout[2],
        );
    }

    fn filter_iter(&self) -> impl Iterator<Item = &RenderedLine> {
        self.lines
            .iter()
            .filter(move |l| self.filter.matches(&l.plain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{LogLine, LogStream};

    fn line(container: &str, stream: LogStream, message: &str) -> LogLine {
        LogLine {
            container: container.into(),
            stream,
            message: message.into(),
        }
    }

    #[test]
    fn push_appends_lines() {
        let mut s = LogsState::new();
        s.push_line(&line("c1", LogStream::Stdout, "hello"));
        s.push_line(&line("c1", LogStream::Stderr, "world"));
        assert_eq!(s.lines.len(), 2);
    }

    #[test]
    fn push_caps_to_line_limit() {
        let mut s = LogsState::new();
        for i in 0..(LINE_LIMIT + 50) {
            s.push_line(&line("c", LogStream::Stdout, &format!("line {i}")));
        }
        assert_eq!(s.lines.len(), LINE_LIMIT);
    }

    #[test]
    fn clear_resets_scroll_and_follow() {
        let mut s = LogsState::new();
        s.push_line(&line("c", LogStream::Stdout, "x"));
        s.scroll_up(5);
        assert!(!s.auto_follow);
        s.clear();
        assert!(s.lines.is_empty());
        assert!(s.auto_follow);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn scroll_up_disables_follow() {
        let mut s = LogsState::new();
        assert!(s.auto_follow);
        s.scroll_up(3);
        assert!(!s.auto_follow);
        assert_eq!(s.scroll, 0); // already at 0; clamped
        s.scroll_down(5);
        assert_eq!(s.scroll, 5);
    }

    #[test]
    fn jump_to_bottom_re_enables_follow() {
        let mut s = LogsState::new();
        s.scroll_up(3);
        s.scroll_down(7);
        s.jump_to_bottom();
        assert!(s.auto_follow);
    }

    #[test]
    fn filter_input_lifecycle() {
        let mut s = LogsState::new();
        assert!(!s.input_mode());
        s.begin_filter_input();
        assert!(s.input_mode());
        s.filter_push_char('e');
        s.filter_push_char('r');
        s.filter_push_char('r');
        s.filter_apply();
        assert!(!s.input_mode());
        assert_eq!(s.current_filter(), Some("err"));
    }

    #[test]
    fn filter_apply_with_empty_buffer_clears_filter() {
        let mut s = LogsState::new();
        s.begin_filter_input();
        s.filter_push_char('x');
        s.filter_apply();
        assert_eq!(s.current_filter(), Some("x"));
        s.begin_filter_input();
        s.filter_backspace();
        s.filter_apply();
        assert_eq!(s.current_filter(), None);
    }

    #[test]
    fn filter_cancel_drops_buffer() {
        let mut s = LogsState::new();
        s.begin_filter_input();
        s.filter_push_char('x');
        s.filter_cancel();
        assert!(!s.input_mode());
        assert_eq!(s.current_filter(), None);
    }

    #[test]
    fn level_cycle_walks_full_set_and_loops() {
        let mut s = LogsState::new();
        assert_eq!(s.level(), LogLevel::All);
        let order = [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warning,
            LogLevel::Error,
            LogLevel::All,
        ];
        for expected in order {
            s.cycle_level();
            assert_eq!(s.level(), expected);
        }
    }

    #[test]
    fn level_hl_arg_emits_none_for_all_some_for_others() {
        assert_eq!(LogLevel::All.hl_arg(), None);
        assert_eq!(LogLevel::Trace.hl_arg(), Some("trace"));
        assert_eq!(LogLevel::Debug.hl_arg(), Some("debug"));
        assert_eq!(LogLevel::Info.hl_arg(), Some("info"));
        assert_eq!(LogLevel::Warning.hl_arg(), Some("warning"));
        assert_eq!(LogLevel::Error.hl_arg(), Some("error"));
    }

    #[test]
    fn level_cycle_clears_buffer() {
        let mut s = LogsState::new();
        s.push_line(&line("c", LogStream::Stdout, "hi"));
        assert_eq!(s.lines.len(), 1);
        s.cycle_level();
        assert!(s.lines.is_empty(), "cycling level must clear stale lines");
    }

    #[test]
    fn filter_iter_matches_substring() {
        let mut s = LogsState::new();
        s.push_line(&line("c", LogStream::Stdout, "GET /health"));
        s.push_line(&line("c", LogStream::Stdout, "POST /api"));
        s.push_line(&line("c", LogStream::Stderr, "GET /api"));
        s.begin_filter_input();
        for c in "/api".chars() {
            s.filter_push_char(c);
        }
        s.filter_apply();
        let kept: Vec<_> = s.filter_iter().collect();
        assert_eq!(kept.len(), 2);
    }
}
