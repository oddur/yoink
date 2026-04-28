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
///
/// No hard cap on the map size — fd / SSH child-process exhaustion
/// would bite long before memory does, and operators don't realistically
/// open dozens of tunnels in a single session. The footer truncates
/// gracefully at the right edge of the terminal regardless.
#[derive(Default)]
pub struct PortForwardState {
    forwards: BTreeMap<ForwardKey, ActiveForward>,
    /// Pre-rendered footer line, rebuilt on every insert/remove/clear
    /// so the per-frame `render_footer` is a single widget render with
    /// no Span allocations. Empty when no forwards are active (the
    /// renderer short-circuits in that case).
    footer_cache: Option<Line<'static>>,
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
    /// Owning the ssh child here means the process dies with the App.
    /// `Option` so unit tests can build the surface without spawning
    /// a real ssh; production code always passes `Some(tunnel)`.
    _tunnel: Option<SshTunnel>,
    /// Sidecar container (only Some on the non-published path).
    /// Drop force-removes the alpine/socat container; auto-remove on
    /// docker handles the case where Drop runs after the runtime has
    /// already torn down.
    _sidecar: Option<crate::pf::SidecarHandle>,
}

impl ActiveForward {
    pub fn new(
        host: &Host,
        service: &str,
        endpoint: PublishedEndpoint,
        local_port: u16,
        url: String,
        tunnel: SshTunnel,
        sidecar: Option<crate::pf::SidecarHandle>,
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
            _sidecar: sidecar,
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
            _sidecar: None,
        }
    }
}

impl PortForwardState {
    pub fn insert(&mut self, fwd: ActiveForward) {
        self.forwards.insert(fwd.key.clone(), fwd);
        self.refresh_footer();
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
        self.refresh_footer();
    }

    /// Close every active forward. Sync — kept around for tests +
    /// non-async callers; production paths use `close_all_async`
    /// so sidecar removes actually complete.
    #[cfg(test)]
    fn clear(&mut self) {
        self.forwards.clear();
        self.refresh_footer();
    }

    /// Like `clear`, but awaits each sidecar's force-remove before
    /// returning so the operator never sees stranded `yoink-pf-*`
    /// containers in `docker ps`. Drains the map first to release
    /// the SshTunnel children, then awaits sidecar close() calls
    /// in parallel.
    pub async fn close_all_async(&mut self) {
        let drained: Vec<_> = std::mem::take(&mut self.forwards).into_values().collect();
        self.refresh_footer();
        let closes: Vec<_> = drained
            .into_iter()
            .filter_map(|fwd| fwd._sidecar.map(crate::pf::SidecarHandle::close))
            .collect();
        if !closes.is_empty() {
            futures_util::future::join_all(closes).await;
        }
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
        if let Some(line) = &self.footer_cache {
            frame.render_widget(Paragraph::new(line.clone()), area);
        }
    }

    /// Rebuild `footer_cache` from the current map. Called once on
    /// every state mutation (insert/remove/clear) so `render_footer`
    /// — invoked every render tick — does no allocation. Returns
    /// `None` for the empty-map case so the renderer can short-circuit.
    fn refresh_footer(&mut self) {
        if self.forwards.is_empty() {
            self.footer_cache = None;
            return;
        }
        let cyan_bold = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let url_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::UNDERLINED);
        let mut spans = Vec::with_capacity(self.forwards.len() * 5 + 2);
        spans.push(Span::styled("↦ ", cyan_bold));
        for (i, fwd) in self.forwards.values().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(fwd.key.service.clone(), cyan_bold));
            spans.push(Span::styled(
                format!(" :{}", fwd.endpoint.container_port),
                dim,
            ));
            spans.push(Span::styled(" → ", dim));
            spans.push(Span::styled(fwd.url.clone(), url_style));
        }
        spans.push(Span::styled("   [o] open  [F] close all", dim));
        self.footer_cache = Some(Line::from(spans));
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
