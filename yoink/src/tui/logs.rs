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

use super::ui::{bold, pane_layout};

const LINE_LIMIT: usize = 5_000;

#[derive(Default)]
pub struct LogsState {
    lines: Vec<RenderedLine>,
    /// Vertical line offset (lines from top of filtered view) when
    /// `auto_follow` is false. Render clamps to `[0, max_scroll]`.
    scroll: u16,
    /// When true, scroll snaps to the bottom on each render so newest
    /// lines stay in view.
    auto_follow: bool,
    /// Substring filter; lines whose plain text doesn't contain this are
    /// hidden. Empty string is treated as no filter. Updated live on
    /// every keystroke while in input mode (vim-`/` style).
    filter: Option<String>,
    /// `Some(buf)` while the user is typing into the filter prompt.
    input_buffer: Option<String>,
    /// Snapshot of `filter` when input mode began — restored on Esc
    /// so cancelling reverts to the prior view instead of clearing.
    prev_filter: Option<String>,
    /// When true, long lines wrap inside the pane instead of being
    /// truncated at the right edge. Toggled by `w`. Works regardless
    /// of whether the line came from `hl` (styled spans) or the plain
    /// forwarder — `Paragraph::wrap` wraps on the rendered glyph
    /// stream after styling has been applied.
    wrap: bool,
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

    #[cfg(test)]
    pub fn wrap_enabled(&self) -> bool {
        self.wrap
    }

    // ─── filter input ──────────────────────────────────────────────────

    pub fn input_mode(&self) -> bool {
        self.input_buffer.is_some()
    }

    pub fn begin_filter_input(&mut self) {
        self.prev_filter = self.filter.clone();
        self.input_buffer = Some(self.filter.clone().unwrap_or_default());
    }

    pub fn filter_push_char(&mut self, c: char) {
        if let Some(buf) = self.input_buffer.as_mut() {
            buf.push(c);
            self.sync_filter_from_buffer();
        }
    }

    pub fn filter_backspace(&mut self) {
        if let Some(buf) = self.input_buffer.as_mut() {
            buf.pop();
            self.sync_filter_from_buffer();
        }
    }

    /// Commit the filter — exit input mode and re-pin to the bottom
    /// so the newest matching lines are visible. The `filter` itself
    /// is already up to date (each keystroke synced it live).
    pub fn filter_apply(&mut self) {
        if self.input_buffer.is_some() {
            self.input_buffer = None;
            self.prev_filter = None;
            self.auto_follow = true;
            self.scroll = 0;
        }
    }

    /// Abandon the in-progress filter — restore whatever filter was
    /// active before `/` was pressed.
    pub fn filter_cancel(&mut self) {
        self.input_buffer = None;
        self.filter = self.prev_filter.take();
    }

    fn sync_filter_from_buffer(&mut self) {
        let buf = self.input_buffer.clone().unwrap_or_default();
        self.filter = if buf.is_empty() { None } else { Some(buf) };
    }

    #[cfg(test)]
    pub fn current_filter(&self) -> Option<&str> {
        self.filter.as_deref()
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
            "yoink logs · services: {services} · {} lines{} · {}",
            total,
            self.filter
                .as_deref()
                .map(|f| format!(" (filter: {f})"))
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
            // `wrap.trim: false` keeps leading whitespace inside a
            // wrapped line — important when a logger emits indented
            // continuation lines (stack traces, structured-log
            // multi-line values) that should stay visually aligned.
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

        let footer = if let Some(buf) = &self.input_buffer {
            Paragraph::new(format!("/{buf}_  (live · enter keep · esc revert)"))
                .style(Style::default().fg(Color::Yellow))
        } else {
            let wrap_hint = if self.wrap { "w wrap*" } else { "w wrap" };
            Paragraph::new(format!(
                "q quit · k clear · / filter · ↑↓ scroll · g top · G bottom · {wrap_hint} · d dashboard · h hosts",
            ))
            .style(Style::default().fg(Color::DarkGray))
        };
        frame.render_widget(footer, layout[2]);
    }

    fn filter_iter(&self) -> impl Iterator<Item = &RenderedLine> {
        let filter = self.filter.clone();
        self.lines.iter().filter(move |l| match &filter {
            Some(f) => l.plain.contains(f.as_str()),
            None => true,
        })
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
    fn filter_iter_matches_substring() {
        let mut s = LogsState::new();
        s.push_line(&line("c", LogStream::Stdout, "GET /health"));
        s.push_line(&line("c", LogStream::Stdout, "POST /api"));
        s.push_line(&line("c", LogStream::Stderr, "GET /api"));
        s.filter = Some("/api".into());
        let kept: Vec<_> = s.filter_iter().collect();
        assert_eq!(kept.len(), 2);
    }
}
