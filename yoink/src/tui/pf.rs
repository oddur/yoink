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
    /// The specific container the operator focused when they
    /// pressed `f`. Used to scope the `↦` row marker to that one
    /// replica instead of lighting up every replica of the service.
    /// `None` for CLI invocations or when the focused row didn't
    /// resolve to a specific container — those fall back to the
    /// per-service mark.
    pub target_container: Option<String>,
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
        target_container: Option<String>,
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
            target_container,
            _tunnel: Some(tunnel),
            _sidecar: sidecar,
        }
    }

    #[cfg(test)]
    fn for_test(host: &str, service: &str, port: u16, local: u16) -> Self {
        Self::for_test_with_container(host, service, port, local, None)
    }

    #[cfg(test)]
    fn for_test_with_container(
        host: &str,
        service: &str,
        port: u16,
        local: u16,
        target_container: Option<&str>,
    ) -> Self {
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
            target_container: target_container.map(str::to_string),
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

    /// Any active forward at all — fallback for the global `o` key
    /// when the operator is on a view without a clear "focused
    /// service" (Hosts list, Logs, Resources). Picks the BTreeMap-
    /// first forward (deterministic; matches the footer's first
    /// entry).
    #[must_use]
    pub fn first(&self) -> Option<&ActiveForward> {
        self.forwards.values().next()
    }

    /// `true` when the service has at least one open forward. Used
    /// by row renderers to prefix a `↦` marker on the service cell
    /// so operators can see at a glance which rows are tunneled.
    #[must_use]
    pub fn is_service_forwarded(&self, service: &str) -> bool {
        self.forwards.values().any(|f| f.key.service == service)
    }

    /// `true` when this exact `(host, container)` pair is the target
    /// of an active forward. Lets row renderers light up the specific
    /// replica the operator pressed `f` on instead of every replica
    /// of the service. Falls back to the per-service mark for
    /// forwards that didn't capture a specific container (CLI
    /// invocations).
    #[must_use]
    pub fn is_container_forwarded(&self, host: &str, container: &str) -> bool {
        self.forwards
            .values()
            .any(|f| f.key.host == host && f.target_container.as_deref() == Some(container))
    }

    /// `true` when the service has any active forward AND none of
    /// the tunnels picked a specific container. The renderer uses
    /// this to fall back to the "all replicas marked" behavior for
    /// CLI-opened forwards that don't carry a focused-row hint.
    #[must_use]
    pub fn is_service_forwarded_unscoped(&self, service: &str) -> bool {
        self.forwards
            .values()
            .any(|f| f.key.service == service && f.target_container.is_none())
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
    fn container_scoped_marker_lights_up_only_target_replica() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test_with_container(
            "host-a",
            "web",
            8080,
            9000,
            Some("web-1"),
        ));
        assert!(state.is_container_forwarded("host-a", "web-1"));
        assert!(!state.is_container_forwarded("host-a", "web-2"));
        // Other replica of the same service is not "unscoped" because
        // the active forward DID pick a specific container.
        assert!(!state.is_service_forwarded_unscoped("web"));
        // The broad per-service predicate still matches (CLI / footer use it).
        assert!(state.is_service_forwarded("web"));
    }

    #[test]
    fn cli_forward_with_no_focus_falls_back_to_unscoped_per_service() {
        let mut state = PortForwardState::default();
        state.insert(ActiveForward::for_test("host-a", "web", 8080, 9000));
        // No specific container: every replica row should mark via
        // `is_service_forwarded_unscoped` fallback.
        assert!(state.is_service_forwarded_unscoped("web"));
        assert!(!state.is_container_forwarded("host-a", "web-1"));
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
