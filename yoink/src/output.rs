//! Human-readable formatters for the CLI. Two flavors:
//!  - `format_status_table` — one row per (host, container), tabled.
//!  - `format_deploy_event` — one short line per event, suitable for
//!    piping through `task yoink:run -- deploy` or echoing to stderr.

/// Render a byte count as `512 MiB`, `1.5 GiB`, etc. Uses 1024-base
/// units; rounds GiB to one decimal so the most common case is readable.
///
/// `i64`→`f64` is lossy beyond ~9 PiB which we'll never have to worry
/// about for memory or single-container counters.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn format_bytes(bytes: i64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let b = bytes as f64;
    if b >= TIB {
        format!("{:.1} TiB", b / TIB)
    } else if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Render a Unix epoch as a compact relative duration ("5s", "12m",
/// "2h", "3d"). Uses the wall clock at call time as the reference,
/// so calling repeatedly on the same value tracks the container
/// aging in the dashboard. Returns "-" for `None` and "0s" for
/// future-dated values (clock skew between operator and host).
#[must_use]
pub fn format_relative_time(epoch_unix: Option<i64>) -> String {
    let Some(then) = epoch_unix else {
        return "-".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let diff = now.saturating_sub(then).max(0);
    if diff < 60 {
        format!("{diff}s")
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86_400)
    }
}

use tabled::Table;
use tabled::Tabled;
use tabled::settings::Style;

use crate::deploy::{DeployEvent, ServiceDeployReport};
use crate::status::StatusReport;

#[derive(Tabled)]
struct StatusRow {
    host: String,
    service: String,
    container: String,
    state: String,
    health: String,
    version: String,
    created: String,
}

/// Pretty-print a `StatusReport` as a table. Empty hosts produce a single
/// row with `(none)` placeholders so the user sees them explicitly.
#[must_use]
pub fn format_status_table(report: &StatusReport) -> String {
    let mut rows: Vec<StatusRow> = Vec::new();
    for host in &report.hosts {
        if host.containers.is_empty() {
            rows.push(StatusRow {
                host: host.host.clone(),
                service: "-".into(),
                container: "(none)".into(),
                state: "-".into(),
                health: "-".into(),
                version: "-".into(),
                created: "-".into(),
            });
            continue;
        }
        for c in &host.containers {
            rows.push(StatusRow {
                host: host.host.clone(),
                service: c.yoink_service.clone().unwrap_or_else(|| "-".into()),
                container: c.name.clone(),
                state: c.state.clone(),
                health: c.health_hint().unwrap_or("-").into(),
                version: c.yoink_version.clone().unwrap_or_else(|| "-".into()),
                created: format_relative_time(c.created_unix),
            });
        }
    }
    let mut table = Table::new(rows);
    table.with(Style::psql());
    format!("{table}")
}

/// One-line summary of a `DeployEvent`, suitable for `eprintln!`.
/// `service` is the optional service-context tag the reconcile loop
/// supplies for events emitted while deploying a specific service —
/// used to prefix lines with `[svc]` so concurrent-wave logs stay
/// visually separable. `None` for top-level events (network setup,
/// global hooks, prefetch).
#[must_use]
pub fn format_deploy_event(service: Option<&str>, event: &DeployEvent) -> String {
    let line = format_deploy_event_body(event);
    match service {
        Some(s) => format!("[{s}] {line}"),
        None => line,
    }
}

fn format_deploy_event_body(event: &DeployEvent) -> String {
    match event {
        DeployEvent::Started { service, tag, host } => {
            format!("[{host}] deploying {service}:{tag}")
        }
        DeployEvent::HookStarted { name } => format!("running pre-deploy hook {name}"),
        DeployEvent::HookFinished { name } => format!("hook {name} complete"),
        DeployEvent::HealthcheckSkipped { host, container } => {
            format!("[{host}] {container} healthcheck skipped (none configured)")
        }
        DeployEvent::NetworkReady {
            host,
            network,
            created,
        } => {
            let suffix = if *created { "(created)" } else { "(exists)" };
            format!("[{host}] network {network} ready {suffix}")
        }
        DeployEvent::PullStarted { host, image, tag } => {
            format!("[{host}] pulling {image}:{tag}")
        }
        DeployEvent::PullFinished { host } => format!("[{host}] pull complete"),
        DeployEvent::ContainerStarted { host, container } => {
            format!("[{host}] started {container}")
        }
        DeployEvent::HealthcheckHealthy {
            host,
            container,
            attempts,
        } => format!("[{host}] {container} healthy ({attempts} attempt(s))"),
        DeployEvent::OldContainerStopped { host, container } => {
            format!("[{host}] stopped old {container}")
        }
        DeployEvent::AlreadyAtSpec { host, container } => {
            format!("[{host}] {container} already at spec — no-op")
        }
        DeployEvent::ContainerLogTail { host, container, lines } => {
            // Multi-line: header + each log line indented so it
            // stands out from the surrounding deploy events.
            let mut out = format!("[{host}] {container} log tail (last {} lines):", lines.len());
            for l in lines {
                out.push_str("\n    ");
                out.push_str(l.trim_end_matches('\n'));
            }
            out
        }
        DeployEvent::Done { host, container } => format!("[{host}] done — {container}"),
    }
}

/// Multi-line summary printed at the end of a successful reconcile.
#[must_use]
pub fn format_deploy_summary(reports: &[ServiceDeployReport]) -> String {
    use std::fmt::Write;
    let mut out = format!("✓ reconciled {} service(s)", reports.len());
    for report in reports {
        write!(out, "\n  {}:{}", report.service, report.tag).expect("write to String never fails");
        for h in &report.hosts {
            write!(
                out,
                "\n    {}: {} (healthy after {} attempt(s); stopped {} old)",
                h.host,
                h.container,
                h.healthcheck_attempts,
                h.stopped_old.len()
            )
            .expect("write to String never fails");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::ContainerInfo;
    use crate::status::{HostStatus, StatusReport};
    use std::collections::BTreeMap;

    fn sample_report() -> StatusReport {
        StatusReport {
            hosts: vec![
                HostStatus {
                    host: "host-a".into(),
                    containers: vec![ContainerInfo {
                        host: "host-a".into(),
                        name: "app-a-a1b2c3d".into(),
                        image: String::new(),
                        state: "running".into(),
                        status_text: "Up 2 hours (healthy)".into(),
                        created_unix: Some(1_735_128_000),
                        yoink_service: Some("app-a".into()),
                        yoink_version: Some("a1b2c3d".into()),
                        yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
                        other_labels: BTreeMap::new(),
                    }],
                },
                HostStatus {
                    host: "host-b".into(),
                    containers: vec![],
                },
            ],
        }
    }

    #[test]
    fn status_table_renders_running_and_empty_host() {
        let s = format_status_table(&sample_report());
        assert!(s.contains("app-a-a1b2c3d"));
        assert!(s.contains("healthy"));
        assert!(s.contains("(none)"));
    }

    #[test]
    fn format_bytes_humanizes_to_kib_mib_gib() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2 * 1024), "2 KiB");
        assert_eq!(format_bytes(512 * 1024 * 1024), "512 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes(32 * 1024 * 1024 * 1024), "32.0 GiB");
    }

    #[test]
    fn deploy_event_format_started() {
        assert_eq!(
            format_deploy_event(None, &DeployEvent::Started {
                service: "app-a".into(),
                tag: "a1b2c3d".into(),
                host: "host-a".into(),
            }),
            "[host-a] deploying app-a:a1b2c3d"
        );
    }

    #[test]
    fn deploy_event_format_network_created_vs_exists() {
        let created = format_deploy_event(None, &DeployEvent::NetworkReady {
            host: "h".into(),
            network: "yoink".into(),
            created: true,
        });
        assert!(created.contains("(created)"));
        let existed = format_deploy_event(None, &DeployEvent::NetworkReady {
            host: "h".into(),
            network: "yoink".into(),
            created: false,
        });
        assert!(existed.contains("(exists)"));
    }

    #[test]
    fn deploy_event_format_healthcheck_healthy() {
        assert_eq!(
            format_deploy_event(None, &DeployEvent::HealthcheckHealthy {
                host: "h".into(),
                container: "c".into(),
                attempts: 3,
            }),
            "[h] c healthy (3 attempt(s))"
        );
    }

    #[test]
    fn summary_lists_each_service_and_host() {
        let reports = vec![crate::deploy::ServiceDeployReport {
            service: "app-a".into(),
            tag: "a1b2c3d".into(),
            hosts: vec![crate::deploy::HostDeployResult {
                host: "host-a".into(),
                container: "app-a-a1b2c3d".into(),
                healthcheck_attempts: 2,
                stopped_old: vec!["app-a-9f8e7d6".into()],
            }],
        }];
        let out = format_deploy_summary(&reports);
        assert!(out.contains("✓ reconciled 1 service(s)"));
        assert!(out.contains("app-a:a1b2c3d"));
        assert!(out.contains("host-a: app-a-a1b2c3d"));
        assert!(out.contains("healthy after 2 attempt(s)"));
        assert!(out.contains("stopped 1 old"));
    }
}
