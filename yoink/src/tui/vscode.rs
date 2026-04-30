//! TUI VS-Code-in-browser sessions — opens / lists / closes the
//! `yoink vscode` sidecar + SSH tunnel without leaving the dashboard.
//! Mirrors the CLI shape: one sidecar per (host, service) pair, served
//! at `http://localhost:N/?folder=/proc/1/root`.
//!
//! Lifecycle: each `ActiveSession` owns its `SshTunnel` child and the
//! `SidecarHandle` that force-removes the code-server container on
//! close. App teardown drains the map via `close_all_async`.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::pf::SidecarHandle;
use crate::transport::tunnel::SshTunnel;

#[derive(Default)]
pub struct VscodeSessionState {
    sessions: BTreeMap<SessionKey, ActiveSession>,
    /// Pre-rendered footer line, rebuilt on every insert/remove so
    /// the per-frame render is alloc-free. `None` when empty so the
    /// renderer short-circuits.
    footer_cache: Option<Line<'static>>,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct SessionKey {
    pub host: String,
    pub service: String,
}

pub struct ActiveSession {
    pub key: SessionKey,
    pub url: String,
    /// SSH tunnel: drop kills the `ssh -L` child.
    _tunnel: Option<SshTunnel>,
    /// code-server sidecar: drop / `close()` force-removes the
    /// container. `Option` so tests can build sessions without
    /// spawning real docker calls.
    sidecar: Option<SidecarHandle>,
}

impl ActiveSession {
    pub fn new(
        host: &str,
        service: &str,
        url: String,
        tunnel: SshTunnel,
        sidecar: SidecarHandle,
    ) -> Self {
        Self {
            key: SessionKey {
                host: host.to_string(),
                service: service.to_string(),
            },
            url,
            _tunnel: Some(tunnel),
            sidecar: Some(sidecar),
        }
    }

    #[cfg(test)]
    fn for_test(host: &str, service: &str, port: u16) -> Self {
        Self {
            key: SessionKey {
                host: host.into(),
                service: service.into(),
            },
            url: format!("http://localhost:{port}/?folder=/proc/1/root"),
            _tunnel: None,
            sidecar: None,
        }
    }
}

impl VscodeSessionState {
    pub fn insert(&mut self, session: ActiveSession) {
        self.sessions.insert(session.key.clone(), session);
        self.refresh_footer();
    }

    #[must_use]
    pub fn get(&self, host: &str, service: &str) -> Option<&ActiveSession> {
        self.sessions.get(&SessionKey {
            host: host.to_string(),
            service: service.to_string(),
        })
    }

    /// Predicate for the row marker (`◊`) on service rows that have
    /// an active vscode session.
    #[must_use]
    pub fn is_service_active(&self, service: &str) -> bool {
        self.sessions.values().any(|s| s.key.service == service)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Drain + force-remove every sidecar in parallel. Called on TUI
    /// quit so operators don't strand `yoink-vscode-*` containers.
    pub async fn close_all_async(&mut self) {
        let drained: Vec<_> = std::mem::take(&mut self.sessions).into_values().collect();
        self.refresh_footer();
        let closes: Vec<_> = drained
            .into_iter()
            .filter_map(|s| s.sidecar.map(SidecarHandle::close))
            .collect();
        if !closes.is_empty() {
            futures_util::future::join_all(closes).await;
        }
    }

    pub fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(line) = &self.footer_cache {
            frame.render_widget(Paragraph::new(line.clone()), area);
        }
    }

    fn refresh_footer(&mut self) {
        if self.sessions.is_empty() {
            self.footer_cache = None;
            return;
        }
        let magenta_bold = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let url_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::UNDERLINED);
        let mut spans = Vec::with_capacity(self.sessions.len() * 4 + 2);
        spans.push(Span::styled("◊ vscode ", magenta_bold));
        for (i, s) in self.sessions.values().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(s.key.service.clone(), magenta_bold));
            spans.push(Span::styled(" → ", dim));
            spans.push(Span::styled(s.url.clone(), url_style));
        }
        spans.push(Span::styled("   [V] close vscode", dim));
        self.footer_cache = Some(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup() {
        let mut state = VscodeSessionState::default();
        state.insert(ActiveSession::for_test("host-a", "api", 7891));
        assert!(state.get("host-a", "api").is_some());
        assert!(state.get("host-b", "api").is_none());
        assert!(state.is_service_active("api"));
        assert!(!state.is_service_active("web"));
    }

    #[test]
    fn second_insert_for_same_key_replaces() {
        let mut state = VscodeSessionState::default();
        state.insert(ActiveSession::for_test("host-a", "api", 7891));
        state.insert(ActiveSession::for_test("host-a", "api", 7892));
        assert_eq!(state.len(), 1);
        assert!(state.get("host-a", "api").unwrap().url.contains("7892"));
    }
}
