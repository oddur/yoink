//! TUI port-forward state — opens / lists / closes `ssh -L` tunnels
//! against published service ports without leaving the dashboard.
//! Mirrors the CLI `yoink pf` shape: only services with a `publish:`
//! entry are forwardable; services without get a "use yoink shell"
//! toast.
//!
//! Lifecycle: each `ActiveForward` owns its `SshTunnel` child, so
//! Drop on the App (TUI exit, panic) tears every tunnel down. Per-
//! tunnel keys (host, service, container_port) are unique — `f` on
//! the same row twice is a no-op (the toast notes it's already up).

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::docker_ops::Host;
use crate::pf::PublishedEndpoint;
use crate::transport::tunnel::SshTunnel;

/// In-app collection of live forwards. Lookups are by `(host_address,
/// service, container_port)`; the BTreeMap ordering gives us a stable
/// render order in the footer.
#[derive(Default)]
pub struct PortForwardState {
    forwards: BTreeMap<ForwardKey, ActiveForward>,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct ForwardKey {
    pub host: String,
    pub service: String,
    pub container_port: u16,
}

pub struct ActiveForward {
    pub key: ForwardKey,
    pub endpoint: PublishedEndpoint,
    /// OS-assigned local port the SSH tunnel bound. Stored here as
    /// the source of truth alongside `url` so a future "Tunnels"
    /// pane can render either projection without recomputing.
    #[allow(dead_code)]
    pub local_port: u16,
    pub url: String,
    /// Owning the child here means the ssh process dies with the App.
    /// `Option` so unit tests can build the surface without spawning
    /// a real ssh; production code always passes `Some(tunnel)`.
    _tunnel: Option<SshTunnel>,
}

impl ActiveForward {
    pub fn new(
        host: &Host,
        service: &str,
        endpoint: PublishedEndpoint,
        local_port: u16,
        url: String,
        tunnel: SshTunnel,
    ) -> Self {
        Self {
            key: ForwardKey {
                host: host.address.clone(),
                service: service.to_string(),
                container_port: endpoint.container_port,
            },
            endpoint,
            local_port,
            url,
            _tunnel: Some(tunnel),
        }
    }

    #[cfg(test)]
    fn for_test(host: &str, service: &str, port: u16, local: u16) -> Self {
        Self {
            key: ForwardKey {
                host: host.into(),
                service: service.into(),
                container_port: port,
            },
            endpoint: PublishedEndpoint {
                host_ip: "127.0.0.1".into(),
                host_port: local,
                container_port: port,
            },
            local_port: local,
            url: format!("http://localhost:{local}"),
            _tunnel: None,
        }
    }
}

impl PortForwardState {
    pub fn insert(&mut self, fwd: ActiveForward) {
        self.forwards.insert(fwd.key.clone(), fwd);
    }

    /// Returns the existing forward for `(host, service, port)` if
    /// any. Used by the `f` key handler to avoid stacking tunnels and
    /// by the footer renderer to pick a row's URL for `o`.
    #[must_use]
    pub fn get(&self, host: &str, service: &str, container_port: u16) -> Option<&ActiveForward> {
        self.forwards.get(&ForwardKey {
            host: host.to_string(),
            service: service.to_string(),
            container_port,
        })
    }

    /// First active forward for a service (any host, any port). The
    /// dashboard's row-focused `o` opens this URL — operators usually
    /// only have one forward per service open at a time, so picking
    /// the first is right in practice.
    #[must_use]
    pub fn first_for_service(&self, service: &str) -> Option<&ActiveForward> {
        self.forwards.values().find(|f| f.key.service == service)
    }

    /// Drop a single tunnel. Wired up but not yet keyed (the focused-
    /// row remove gesture lands in the follow-up "Tunnels pane" PR).
    #[allow(dead_code)]
    pub fn remove(&mut self, host: &str, service: &str, container_port: u16) {
        self.forwards.remove(&ForwardKey {
            host: host.to_string(),
            service: service.to_string(),
            container_port,
        });
    }

    /// Close every active forward. Invoked by `Shift-F` from the
    /// dashboard or on App drop.
    pub fn clear(&mut self) {
        self.forwards.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forwards.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.forwards.len()
    }

    /// One-line footer: `↦ pgadmin :80 → http://localhost:5050  api :8080 → localhost:54321 …  [o] open  [F] close all`.
    /// Renders into a single-row strip; ratatui truncates if the
    /// operator has too many tunnels open. Goal: an open tunnel
    /// is impossible to forget — the band stays on every pane.
    pub fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.forwards.is_empty() {
            return;
        }
        let mut spans = vec![Span::styled(
            "↦ ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        for (i, fwd) in self.forwards.values().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ", Style::default()));
            }
            spans.push(Span::styled(
                fwd.key.service.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" :{}", fwd.endpoint.container_port),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(" → ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                fwd.url.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::UNDERLINED),
            ));
        }
        spans.push(Span::styled(
            "   [o] open  [F] close all",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_keys_isolate_by_host_service_port() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test("host-a", "pgadmin", 80, 5050));
        assert!(state.get("host-a", "pgadmin", 80).is_some());
        assert!(state.get("host-b", "pgadmin", 80).is_none());
        state.insert(ActiveForward::for_test("host-b", "pgadmin", 80, 5051));
        assert!(state.get("host-b", "pgadmin", 80).is_some());
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn first_for_service_picks_any_host() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test("host-b", "api", 8080, 9001));
        state.insert(ActiveForward::for_test("host-a", "api", 8080, 9000));
        let first = state.first_for_service("api").unwrap();
        // BTreeMap ordering on (host, service, port) puts host-a first.
        assert_eq!(first.key.host, "host-a");
    }

    #[test]
    fn clear_removes_everything() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test("host-a", "pgadmin", 80, 5050));
        state.insert(ActiveForward::for_test("host-a", "api", 8080, 9000));
        assert_eq!(state.len(), 2);
        state.clear();
        assert!(state.is_empty());
    }

    #[test]
    fn remove_targeted_one() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test("host-a", "pgadmin", 80, 5050));
        state.insert(ActiveForward::for_test("host-a", "api", 8080, 9000));
        state.remove("host-a", "api", 8080);
        assert_eq!(state.len(), 1);
        assert!(state.get("host-a", "pgadmin", 80).is_some());
    }
}
