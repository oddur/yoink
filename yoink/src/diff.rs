//! Static diff between configured services and what's running on each
//! host. Powers `yoink up --dry-run` and the PR-comment dry-run path
//! that CI workflows post into pull requests before merge.
//!
//! The diff is purely informational — `compute` never mutates the
//! host. It reuses the same spec-hash logic the reconcile loop uses
//! for its skip-if-already-at-spec optimization, so what `compute`
//! reports lines up exactly with what `yoink up` would (or wouldn't)
//! actually do.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;

use crate::config::Config;
use crate::deploy::{self, DeployError, build_desired_spec, container_name};
use crate::docker;
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, Host};
use crate::secrets::SecretsBundle;

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error(transparent)]
    Deploy(#[from] DeployError),
}

/// Aggregate result. One row per (service, host) pair, plus a list of
/// yoink-managed containers no declared service owns.
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub services: Vec<ServiceDiff>,
    pub orphans: Vec<OrphanContainer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceDiff {
    pub service: String,
    pub host: String,
    /// Resolved tag (from --tag override, then service.tag).
    pub tag: String,
    pub desired_hash: String,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChangeKind {
    /// No matching container with the same `yoink.service` label.
    Create { desired_image: String },
    /// Existing container has the service label but a different
    /// `spec_hash`. `fields` is empty when we couldn't inspect the
    /// running container (transient docker error) — the hash drift
    /// is still authoritative; only the per-field breakdown is
    /// best-effort.
    Update {
        current_hash: String,
        current_image: String,
        desired_image: String,
        #[serde(default, skip_serializing_if = "FieldDiff::is_empty")]
        fields: FieldDiff,
    },
    /// Spec hash matches — nothing for `yoink up` to do.
    NoOp { current_hash: String },
}

/// Per-field diff between a running container and its desired spec.
/// Keys only — values may carry secret material in env, and hashes
/// hide it for labels too. Operators get "what changed" without
/// risking a sealed secret leaking into a PR comment.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FieldDiff {
    pub env_added: Vec<String>,
    pub env_removed: Vec<String>,
    pub env_changed: Vec<String>,
    pub labels_added: Vec<String>,
    pub labels_removed: Vec<String>,
    pub labels_changed: Vec<String>,
}

impl FieldDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env_added.is_empty()
            && self.env_removed.is_empty()
            && self.env_changed.is_empty()
            && self.labels_added.is_empty()
            && self.labels_removed.is_empty()
            && self.labels_changed.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrphanContainer {
    pub host: String,
    pub name: String,
    /// `yoink.service` label, if any. `None` for ancient containers
    /// from before the label existed.
    pub service: Option<String>,
}

/// `(create, update, noop, orphans)` rollup, useful for the PR-comment
/// "Plan: …" line and exit-code logic in CI.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Summary {
    pub create: usize,
    pub update: usize,
    pub noop: usize,
    pub orphans: usize,
}

impl DiffReport {
    #[must_use]
    pub fn summary(&self) -> Summary {
        let mut s = Summary {
            create: 0,
            update: 0,
            noop: 0,
            orphans: self.orphans.len(),
        };
        for d in &self.services {
            match d.change {
                ChangeKind::Create { .. } => s.create += 1,
                ChangeKind::Update { .. } => s.update += 1,
                ChangeKind::NoOp { .. } => s.noop += 1,
            }
        }
        s
    }

    /// Are there real changes (not counting orphans, which `yoink up`
    /// won't touch)?
    #[must_use]
    pub fn has_changes(&self) -> bool {
        let s = self.summary();
        s.create + s.update > 0
    }
}

#[allow(clippy::too_many_lines)] // single-pass diff over services × hosts; splitting hides the data flow
pub async fn compute(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
) -> Result<DiffReport, DiffError> {
    // Snapshot every host's yoink-managed containers in parallel —
    // for an N-host fleet this is one round-trip instead of N. Same
    // fan-out shape `deploy::reconcile` and the prune path use.
    let snapshot: BTreeMap<String, Vec<ContainerInfo>> = {
        let futs = config.hosts.iter().map(|host_cfg| {
            let host = Host::from(host_cfg);
            async move {
                let containers = ops
                    .list_containers_by_label(&host, "yoink.managed=true")
                    .await
                    .map_err(|source| DiffError::Docker {
                        host: host.address.clone(),
                        source,
                    })?;
                Ok::<_, DiffError>((host.address, containers))
            }
        });
        futures_util::future::try_join_all(futs)
            .await?
            .into_iter()
            .collect()
    };

    let selected: BTreeSet<&str> = match services_filter {
        Some(f) => f.iter().map(String::as_str).collect(),
        None => config.services.iter().map(|s| s.name.as_str()).collect(),
    };

    let mut services = Vec::new();
    // Track which containers we've claimed. Anything left at the end
    // becomes an orphan.
    let mut owned: BTreeSet<(String, String)> = BTreeSet::new();

    for svc in &config.services {
        if !selected.contains(svc.name.as_str()) {
            continue;
        }
        let tag = tag_overrides
            .get(&svc.name)
            .cloned()
            .or_else(|| svc.tag.clone())
            .ok_or(DeployError::TagMissing {
                service: svc.name.clone(),
            })?;
        let desired_spec = build_desired_spec(config, svc, &tag, secrets)?;
        let desired_hash = docker::compute_spec_hash(&desired_spec);
        let desired_image = docker::image_reference(&svc.image, &tag);

        for host_cfg in svc.applicable_hosts(&config.hosts) {
            let host = host_cfg.address.clone();
            let containers = snapshot.get(&host).map_or(&[][..], Vec::as_slice);

            // Claim every desired-spec replica that's already running
            // with the matching spec_hash — that's the no-op path.
            let mut all_present = true;
            for index in 0..svc.run.replicas {
                let name = container_name(&svc.name, &desired_hash, index, svc.run.replicas);
                let matched = containers.iter().any(|c| {
                    c.name == name
                        && c.is_running()
                        && c.yoink_spec_hash.as_deref() == Some(desired_hash.as_str())
                });
                if matched {
                    owned.insert((host.clone(), name));
                } else {
                    all_present = false;
                }
            }

            let change = if all_present {
                ChangeKind::NoOp {
                    current_hash: desired_hash.clone(),
                }
            } else {
                // CREATE vs UPDATE: is there *any* running container
                // labeled for this service? If yes, it's about to be
                // swapped — claim it so it's not flagged as orphan.
                let existing = containers.iter().find(|c| {
                    c.yoink_service.as_deref() == Some(svc.name.as_str()) && c.is_running()
                });
                match existing {
                    None => ChangeKind::Create {
                        desired_image: desired_image.clone(),
                    },
                    Some(c) => {
                        owned.insert((host.clone(), c.name.clone()));
                        let fields = inspect_field_diff(
                            ops,
                            &Host::from(host_cfg),
                            &c.name,
                            svc,
                            &tag,
                            &desired_hash,
                            secrets,
                        )
                        .await
                        .unwrap_or_default();
                        ChangeKind::Update {
                            current_hash: c.yoink_spec_hash.clone().unwrap_or_default(),
                            current_image: c.image.clone(),
                            desired_image: desired_image.clone(),
                            fields,
                        }
                    }
                }
            };
            services.push(ServiceDiff {
                service: svc.name.clone(),
                host,
                tag: tag.clone(),
                desired_hash: desired_hash.clone(),
                change,
            });
        }
    }

    let mut orphans = Vec::new();
    for (host, containers) in &snapshot {
        for c in containers {
            if !owned.contains(&(host.clone(), c.name.clone())) {
                orphans.push(OrphanContainer {
                    host: host.clone(),
                    name: c.name.clone(),
                    service: c.yoink_service.clone(),
                });
            }
        }
    }

    Ok(DiffReport { services, orphans })
}

/// Inspect the running container, parse its env into a key/value map,
/// and return a key-only diff against the desired env+labels. Errors
/// are swallowed (`Default::default()` returned) — the dry-run is
/// informational and the hash drift is still authoritative.
async fn inspect_field_diff(
    ops: &dyn DockerOps,
    host: &Host,
    container: &str,
    service: &crate::config::ServiceConfig,
    tag: &str,
    desired_hash: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<FieldDiff, DockerError> {
    let detail = ops.inspect_container(host, container).await?;
    let desired_env = deploy::build_env(service, secrets);
    let desired_labels = deploy::build_labels(service, tag, desired_hash);
    Ok(field_diff_scoped(
        service,
        &detail.env_map(),
        &detail.labels,
        &desired_env,
        &desired_labels,
    ))
}

/// Pure-function part of `inspect_field_diff`: given current vs desired
/// env+labels (already loaded), produce the operator-facing field diff,
/// filtered down to keys yoink would actually set.
///
/// The scoping matters because the running container's env always
/// carries image-inherited entries (`PATH`, `SSL_CERT_FILE`, locale
/// vars, …) and the running container's labels carry image-baked OCI
/// annotations (`LABEL service=foo` from a Dockerfile, `org.openconta-
/// iners.image.*`). yoink doesn't manage any of those, so they
/// shouldn't show as drift — they'd flood the modal with permanent
/// red `-` lines. The contract is "yoink reports drift on what it
/// sets": for env that's `service.env` + declared `secrets:` + LHS
/// of `env_from_secrets:`; for labels that's anything starting with
/// `yoink.` or anything the operator declared in `service.labels`.
fn field_diff_scoped(
    service: &crate::config::ServiceConfig,
    current_env: &BTreeMap<String, String>,
    current_labels: &BTreeMap<String, String>,
    desired_env: &BTreeMap<String, String>,
    desired_labels: &BTreeMap<String, String>,
) -> FieldDiff {
    let managed_env: BTreeSet<String> = service
        .env
        .keys()
        .cloned()
        .chain(service.secrets.iter().cloned())
        .chain(service.env_from_secrets.keys().cloned())
        .collect();
    let (mut env_added, mut env_removed, mut env_changed) = diff_kv(current_env, desired_env);
    env_added.retain(|k| managed_env.contains(k));
    env_removed.retain(|k| managed_env.contains(k));
    env_changed.retain(|k| managed_env.contains(k));

    let user_labels: BTreeSet<String> = service.labels.keys().cloned().collect();
    let label_in_scope = |k: &String| {
        (k.starts_with("yoink.") || user_labels.contains(k)) && !is_yoink_internal_label(k)
    };
    let (mut labels_added, mut labels_removed, mut labels_changed) =
        diff_kv(current_labels, desired_labels);
    labels_added.retain(&label_in_scope);
    labels_removed.retain(&label_in_scope);
    labels_changed.retain(&label_in_scope);

    FieldDiff {
        env_added,
        env_removed,
        env_changed,
        labels_added,
        labels_removed,
        labels_changed,
    }
}

/// Yoink-internal labels suppressed from the field diff. These are
/// either represented by other parts of the diff (`yoink.version`,
/// `yoink.spec_hash` ↔ the tag and `spec_hash` columns) or by design
/// absent from `build_labels` (`yoink.deployed-*` audit labels — see
/// `deploy::audit_labels`).
fn is_yoink_internal_label(k: &str) -> bool {
    matches!(k, "yoink.version" | "yoink.spec_hash") || k.starts_with("yoink.deployed-")
}

fn diff_kv(
    current: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (k, v) in desired {
        match current.get(k) {
            None => added.push(k.clone()),
            Some(cv) if cv != v => changed.push(k.clone()),
            _ => {}
        }
    }
    let removed: Vec<String> = current
        .keys()
        .filter(|k| !desired.contains_key(*k))
        .cloned()
        .collect();
    (added, removed, changed)
}

// ---------------------------------------------------------------------
// Rendering — three formats (text default, markdown for PR comments,
// json for machine consumers).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub enum Format {
    #[default]
    Text,
    Markdown,
    Json,
}

impl DiffReport {
    /// Render in the requested format. JSON serialization can't fail
    /// for our types, but if it ever did we surface the error inline
    /// rather than panicking.
    #[must_use]
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Text => self.render_text(),
            Format::Markdown => self.render_markdown(),
            Format::Json => serde_json::to_string_pretty(self)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),
        }
    }

    fn render_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "yoink up — dry run (no changes will be made)\n");

        let by_host = group_by_host(&self.services);
        for (host, rows) in &by_host {
            let _ = writeln!(out, "{host}");
            for d in rows {
                let _ = writeln!(out, "  {}", text_row(d));
            }
            let _ = writeln!(out);
        }

        if !self.orphans.is_empty() {
            let _ = writeln!(out, "orphan containers (run `yoink prune` to remove):");
            for o in &self.orphans {
                let label = match &o.service {
                    Some(s) => format!("{} (service={s})", o.name),
                    None => o.name.clone(),
                };
                let _ = writeln!(out, "  - {}@{label}", o.host);
            }
            let _ = writeln!(out);
        }

        let s = self.summary();
        let _ = writeln!(
            out,
            "Plan: {} to create, {} to update, {} unchanged, {} orphan",
            s.create, s.update, s.noop, s.orphans
        );
        out
    }

    fn render_markdown(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let s = self.summary();
        let _ = writeln!(out, "### yoink dry-run\n");
        let _ = writeln!(
            out,
            "**Plan:** {} to create · {} to update · {} unchanged · {} orphan\n",
            s.create, s.update, s.noop, s.orphans
        );

        let by_host = group_by_host(&self.services);
        for (host, rows) in &by_host {
            let _ = writeln!(out, "#### `{host}`\n");
            let _ = writeln!(out, "| | service | spec | image |");
            let _ = writeln!(out, "|---|---|---|---|");
            for d in rows {
                let (icon, spec, image) = markdown_cells(d);
                let _ = writeln!(out, "| {icon} | `{}` | {spec} | {image} |", d.service);
            }
            let _ = writeln!(out);
        }

        if !self.orphans.is_empty() {
            // Collapse to keep the PR comment scannable when the host
            // has accumulated old generations between prunes.
            let _ = writeln!(
                out,
                "<details><summary>{} orphan container{} (run <code>yoink prune</code> to remove)</summary>\n",
                self.orphans.len(),
                if self.orphans.len() == 1 { "" } else { "s" }
            );
            for o in &self.orphans {
                let svc = o.service.as_deref().unwrap_or("?");
                let _ = writeln!(out, "- `{}@{}` (service: `{svc}`)", o.host, o.name);
            }
            let _ = writeln!(out, "\n</details>\n");
        }

        if !self.has_changes() {
            let _ = writeln!(out, "_No changes._");
        }
        out
    }
}

fn group_by_host(rows: &[ServiceDiff]) -> BTreeMap<&str, Vec<&ServiceDiff>> {
    let mut by: BTreeMap<&str, Vec<&ServiceDiff>> = BTreeMap::new();
    for d in rows {
        by.entry(d.host.as_str()).or_default().push(d);
    }
    by
}

fn short(h: &str) -> &str {
    &h[..h.len().min(7)]
}

fn text_row(d: &ServiceDiff) -> String {
    match &d.change {
        ChangeKind::Create { desired_image } => {
            format!("+ {:18} (new)              {desired_image}", d.service)
        }
        ChangeKind::Update {
            current_hash,
            current_image,
            desired_image,
            fields,
        } => {
            let head = format!(
                "~ {:18} {} → {}    {current_image} → {desired_image}",
                d.service,
                short(current_hash),
                short(&d.desired_hash),
            );
            let detail = field_diff_lines(fields, "      ");
            if detail.is_empty() {
                head
            } else {
                format!("{head}\n{detail}")
            }
        }
        ChangeKind::NoOp { current_hash } => format!(
            "= {:18} {}              (no change)",
            d.service,
            short(current_hash)
        ),
    }
}

/// Render a `FieldDiff` as `+`/`-`/`~` lines, indented by `indent`.
/// Empty when nothing changed at the env/label level.
fn field_diff_lines(fields: &FieldDiff, indent: &str) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut group = |label: &str, added: &[String], removed: &[String], changed: &[String]| {
        if added.is_empty() && removed.is_empty() && changed.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        for k in added {
            parts.push(format!("+ {k}"));
        }
        for k in removed {
            parts.push(format!("- {k}"));
        }
        for k in changed {
            parts.push(format!("~ {k}"));
        }
        lines.push(format!("{indent}{label}: {}", parts.join(", ")));
    };
    group(
        "env",
        &fields.env_added,
        &fields.env_removed,
        &fields.env_changed,
    );
    group(
        "labels",
        &fields.labels_added,
        &fields.labels_removed,
        &fields.labels_changed,
    );
    lines.join("\n")
}

fn markdown_cells(d: &ServiceDiff) -> (&'static str, String, String) {
    match &d.change {
        ChangeKind::Create { desired_image } => ("🟢", "new".into(), format!("`{desired_image}`")),
        ChangeKind::Update {
            current_hash,
            current_image,
            desired_image,
            fields: _,
        } => (
            "🟡",
            format!("`{}` → `{}`", short(current_hash), short(&d.desired_hash)),
            format!("`{current_image}` → `{desired_image}`"),
        ),
        ChangeKind::NoOp { current_hash } => (
            "⚪",
            format!("`{}` (unchanged)", short(current_hash)),
            "_(unchanged)_".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn diff_with(services: Vec<ServiceDiff>, orphans: Vec<OrphanContainer>) -> DiffReport {
        DiffReport { services, orphans }
    }

    fn create(svc: &str, host: &str, image: &str) -> ServiceDiff {
        ServiceDiff {
            service: svc.into(),
            host: host.into(),
            tag: "v1".into(),
            desired_hash: "abcdef0123".into(),
            change: ChangeKind::Create {
                desired_image: image.into(),
            },
        }
    }

    fn update(svc: &str, host: &str) -> ServiceDiff {
        ServiceDiff {
            service: svc.into(),
            host: host.into(),
            tag: "v2".into(),
            desired_hash: "deadbeef00".into(),
            change: ChangeKind::Update {
                current_hash: "0123456789".into(),
                current_image: "img:v1".into(),
                desired_image: "img:v2".into(),
                fields: FieldDiff::default(),
            },
        }
    }

    fn noop(svc: &str, host: &str) -> ServiceDiff {
        ServiceDiff {
            service: svc.into(),
            host: host.into(),
            tag: "v3".into(),
            desired_hash: "cafef00ddd".into(),
            change: ChangeKind::NoOp {
                current_hash: "cafef00ddd".into(),
            },
        }
    }

    #[test]
    fn summary_counts_each_kind_plus_orphans() {
        let r = diff_with(
            vec![
                create("a", "h1", "a:v1"),
                update("b", "h1"),
                update("c", "h1"),
                noop("d", "h1"),
            ],
            vec![OrphanContainer {
                host: "h1".into(),
                name: "old".into(),
                service: Some("legacy".into()),
            }],
        );
        let s = r.summary();
        assert_eq!(s.create, 1);
        assert_eq!(s.update, 2);
        assert_eq!(s.noop, 1);
        assert_eq!(s.orphans, 1);
        assert!(r.has_changes());
    }

    #[test]
    fn has_changes_false_when_all_noop() {
        let r = diff_with(vec![noop("a", "h1"), noop("b", "h1")], vec![]);
        assert!(!r.has_changes());
    }

    #[test]
    fn text_render_includes_each_change_kind() {
        let r = diff_with(
            vec![
                create("a", "h1", "a:v1"),
                update("b", "h1"),
                noop("c", "h1"),
            ],
            vec![],
        );
        let t = r.render(Format::Text);
        assert!(t.contains("+ a"));
        assert!(t.contains("~ b"));
        assert!(t.contains("= c"));
        assert!(t.contains("Plan: 1 to create, 1 to update, 1 unchanged, 0 orphan"));
    }

    #[test]
    fn markdown_render_groups_by_host_and_emits_table() {
        let r = diff_with(
            vec![create("a", "h1", "a:v1"), update("b", "h2")],
            vec![OrphanContainer {
                host: "h1".into(),
                name: "ghost".into(),
                service: None,
            }],
        );
        let m = r.render(Format::Markdown);
        assert!(m.contains("### yoink dry-run"));
        assert!(m.contains("#### `h1`"));
        assert!(m.contains("#### `h2`"));
        assert!(m.contains("| `a` |"));
        assert!(m.contains("| `b` |"));
        assert!(m.contains("<details>"));
        assert!(m.contains("orphan"));
        assert!(m.contains("`h1@ghost`"));
    }

    #[test]
    fn json_render_is_valid_json() {
        let r = diff_with(vec![create("a", "h1", "a:v1")], vec![]);
        let j = r.render(Format::Json);
        let parsed: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["services"][0]["service"], "a");
        assert_eq!(parsed["services"][0]["change"]["kind"], "create");
        assert_eq!(parsed["services"][0]["change"]["desired_image"], "a:v1");
    }

    #[test]
    fn no_changes_markdown_says_so() {
        let r = diff_with(vec![noop("a", "h1")], vec![]);
        let m = r.render(Format::Markdown);
        assert!(m.contains("_No changes._"));
    }

    #[test]
    fn text_render_shows_field_diff_under_update() {
        let mut row = update("api", "h1");
        if let ChangeKind::Update { ref mut fields, .. } = row.change {
            fields.env_added = vec!["DATABASE_POOL_SIZE".into()];
            fields.env_changed = vec!["LOG_LEVEL".into()];
            fields.labels_removed = vec!["yoink.caddy.tls".into()];
        }
        let t = diff_with(vec![row], vec![]).render(Format::Text);
        assert!(t.contains("env: + DATABASE_POOL_SIZE, ~ LOG_LEVEL"));
        assert!(t.contains("labels: - yoink.caddy.tls"));
    }

    #[test]
    fn is_yoink_internal_label_recognises_audit_and_spec_keys() {
        assert!(is_yoink_internal_label("yoink.version"));
        assert!(is_yoink_internal_label("yoink.spec_hash"));
        assert!(is_yoink_internal_label("yoink.deployed-by"));
        assert!(is_yoink_internal_label("yoink.deployed-at"));
        // Other yoink labels (caddy, custom user labels under yoink.*)
        // remain visible in the diff.
        assert!(!is_yoink_internal_label("yoink.caddy.domain"));
        assert!(!is_yoink_internal_label("yoink.service"));
        assert!(!is_yoink_internal_label(
            "org.opencontainers.image.description"
        ));
    }

    fn service_with_managed_keys() -> crate::config::ServiceConfig {
        let cfg = crate::config::Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: api
    image: registry.example.com/bt-api
    tag: latest
    env:
      LOG_LEVEL: info
      PORT: "8080"
    secrets:
      - DATABASE_URL
    env_from_secrets:
      OTEL_HEADERS: GRAFANA_AUTH
    labels:
      ops.team: platform
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
      options: { memory: 256Mi }
"#,
        )
        .unwrap();
        cfg.services[0].clone()
    }

    #[test]
    fn field_diff_ignores_image_inherited_env() {
        // Container has image-baked vars (PATH, SSL_CERT_FILE, NODE_ENV)
        // alongside the ones yoink set. Desired only carries the
        // yoink-set ones. The image-inherited shouldn't show as
        // env_removed.
        let svc = service_with_managed_keys();
        let mut current_env = BTreeMap::new();
        current_env.insert("PATH".into(), "/usr/local/bin:/usr/bin".into());
        current_env.insert("SSL_CERT_FILE".into(), "/etc/ssl/cert.pem".into());
        current_env.insert("NODE_ENV".into(), "production".into());
        current_env.insert("LOG_LEVEL".into(), "info".into());
        current_env.insert("PORT".into(), "8080".into());
        current_env.insert("DATABASE_URL".into(), "postgres://…".into());
        current_env.insert("OTEL_HEADERS".into(), "Bearer …".into());
        let mut desired_env = current_env.clone();
        desired_env.remove("PATH");
        desired_env.remove("SSL_CERT_FILE");
        desired_env.remove("NODE_ENV");
        let diff = field_diff_scoped(
            &svc,
            &current_env,
            &BTreeMap::new(),
            &desired_env,
            &BTreeMap::new(),
        );
        assert!(diff.env_removed.is_empty(), "got: {:?}", diff.env_removed);
    }

    #[test]
    fn field_diff_still_reports_dropped_managed_env() {
        // Operator removes a key from service.env: that IS drift —
        // make sure the scoping doesn't over-filter and silently
        // hide it.
        let svc = service_with_managed_keys();
        let mut current_env = BTreeMap::new();
        current_env.insert("LOG_LEVEL".into(), "info".into());
        current_env.insert("PORT".into(), "8080".into()); // gone in desired
        current_env.insert("PATH".into(), "/usr/bin".into()); // image, ignored
        let mut desired_env = BTreeMap::new();
        desired_env.insert("LOG_LEVEL".into(), "info".into());
        let diff = field_diff_scoped(
            &svc,
            &current_env,
            &BTreeMap::new(),
            &desired_env,
            &BTreeMap::new(),
        );
        assert_eq!(diff.env_removed, vec!["PORT".to_string()]);
    }

    #[test]
    fn field_diff_ignores_image_baked_labels() {
        // bt-api's Dockerfile carries `LABEL service=backtrack` plus
        // OCI image annotations. Those shouldn't show as drift;
        // operator can't act on them.
        let svc = service_with_managed_keys();
        let mut current_labels = BTreeMap::new();
        current_labels.insert("service".into(), "backtrack".into());
        current_labels.insert(
            "org.opencontainers.image.source".into(),
            "https://example.com".into(),
        );
        current_labels.insert("yoink.managed".into(), "true".into());
        current_labels.insert("yoink.service".into(), "api".into());
        current_labels.insert("ops.team".into(), "platform".into());
        let mut desired_labels = current_labels.clone();
        desired_labels.remove("service");
        desired_labels.remove("org.opencontainers.image.source");
        let diff = field_diff_scoped(
            &svc,
            &BTreeMap::new(),
            &current_labels,
            &BTreeMap::new(),
            &desired_labels,
        );
        assert!(
            diff.labels_removed.is_empty(),
            "got: {:?}",
            diff.labels_removed
        );
    }

    #[test]
    fn field_diff_reports_user_label_change() {
        // A label declared in service.labels is yoink-managed; an
        // operator-side change should still show as drift.
        let svc = service_with_managed_keys();
        let mut current_labels = BTreeMap::new();
        current_labels.insert("ops.team".into(), "platform".into());
        let mut desired_labels = BTreeMap::new();
        desired_labels.insert("ops.team".into(), "infra".into());
        let diff = field_diff_scoped(
            &svc,
            &BTreeMap::new(),
            &current_labels,
            &BTreeMap::new(),
            &desired_labels,
        );
        assert_eq!(diff.labels_changed, vec!["ops.team".to_string()]);
    }

    #[test]
    fn diff_kv_classifies_added_removed_changed() {
        let mut current = BTreeMap::new();
        current.insert("KEEP".into(), "same".into());
        current.insert("CHANGE".into(), "old".into());
        current.insert("REMOVE".into(), "gone".into());
        let mut desired = BTreeMap::new();
        desired.insert("KEEP".into(), "same".into());
        desired.insert("CHANGE".into(), "new".into());
        desired.insert("ADD".into(), "fresh".into());
        let (added, removed, changed) = diff_kv(&current, &desired);
        assert_eq!(added, vec!["ADD"]);
        assert_eq!(removed, vec!["REMOVE"]);
        assert_eq!(changed, vec!["CHANGE"]);
    }
}
