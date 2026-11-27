//! On-host audit log. Each yoink-managed host gets an append-only
//! JSONL file at `~/.local/state/yoink/host-audit/events.jsonl` recording every
//! state-changing operation yoink performs there. Surfaced via
//! `yoink audit log|run|gc`.
//!
//! ## Why on-host, not a central store
//!
//! Yoink's bet is "no daemon, no DB". Pushing audit events to a remote
//! aggregator at deploy time would tie operators to a piece of infra
//! they don't otherwise need. Writing to the host filesystem follows
//! the same shape as `/var/lib/yoink/files/` and survives the operator
//! disconnecting mid-deploy.
//!
//! ## Hybrid flush policy
//!
//! - **Per-event flush** for forensic events (`RunStarted`/`RunFinished`,
//!   `ContainerCreated`/`ContainerRemoved`, `Rollback*`, `SecretsRotated`,
//!   `HookFinished`, `LockAcquired`/`LockReleased`, `FileUploaded`). One
//!   SSH `tee -a` per event; tolerates partial runs.
//! - **Batched flush** for progress events (`PullStarted`/`PullFinished`,
//!   `NetworkReady`, `HealthcheckHealthy`, `HookStarted`,
//!   `OldContainerStopped`, `ContainerStarted`, `AlreadyAtSpec`).
//!   Buffered in a per-host ring, flushed at end-of-run (and
//!   explicitly on `flush()`).
//!
//! Audit-write failures never block deploys — they warn to stderr and
//! continue. Forensic events that fail are logged loudly so the
//! operator notices; progress events fail in stderr noise.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::host::Host;

/// Shell expression for the audit directory on a host. It is under
/// the home directory of the SSH user, so a deploy user without root
/// can create it on a server that yoink has never seen. The host shell
/// expands it; quote it as `"$D"` after `D={HOST_AUDIT_DIR_SH}`.
///
/// The directory is not the operator log directory (`yoink/audit`):
/// with the `local` host both live on one machine and must not share
/// a file.
pub const HOST_AUDIT_DIR_SH: &str = "\"${XDG_STATE_HOME:-$HOME/.local/state}/yoink/host-audit\"";
/// Where yoink wrote the host log before the move. Only root can
/// create it. Readers and `audit gc` still cover it, so the history of
/// a host that has one stays visible. yoink no longer writes here.
pub const LEGACY_HOST_AUDIT_DIR: &str = "/var/lib/yoink/audit";

/// The host audit directory on the operator machine, for the `local`
/// host. Same lookup as [`HOST_AUDIT_DIR_SH`].
#[must_use]
pub fn local_host_audit_dir() -> std::path::PathBuf {
    let mut p = operator_audit_dir();
    p.set_file_name("host-audit");
    p
}

/// Active file size at which a flush triggers a rotation. 5 MiB ≈
/// thousands of events; a busy host's audit file rotates every few
/// weeks at most.
pub const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Audit-event payload variants. Discriminated on the `event` JSON
/// field; serde tag = "event" keeps the wire shape flat: every field
/// at the top level, no nested `payload: { ... }`. This matches what
/// JSONL consumers (jq, awk, vector) expect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "PascalCase")]
pub enum AuditEventKind {
    /// Envelope: a `yoink up` / rollback / prune / secrets-rotate run
    /// has begun. One per command invocation, before any host work.
    RunStarted {
        command: String,
        services: Vec<String>,
    },
    /// Envelope: same run finished. `ok: false` carries the operator-
    /// visible error message so a tail of the audit log shows why a
    /// deploy died without grepping terminal scrollback.
    RunFinished {
        command: String,
        ok: bool,
        error: Option<String>,
    },
    /// Lock sentinel created on this host. `LockReleased` always pairs
    /// with this — the gap between them is the deploy-lock hold time.
    LockAcquired,
    LockReleased,
    /// A pre-/post-deploy hook started running. Best-effort.
    HookStarted {
        name: String,
    },
    /// Hook finished. Forensic — the exit code matters for triage.
    HookFinished {
        name: String,
    },
    /// Image pull began on this host. Best-effort progress event.
    PullStarted {
        image: String,
        tag: String,
    },
    PullFinished {
        image: String,
        tag: String,
    },
    /// Docker network ensured (created if missing). Progress event.
    NetworkReady {
        network: String,
        created: bool,
    },
    /// New container started. Progress (the forensic
    /// "this is the container we'll keep" event is `ContainerCreated`,
    /// emitted after the healthcheck passes).
    ContainerStarted {
        service: String,
        container: String,
        spec_hash: String,
        tag: String,
    },
    HealthcheckHealthy {
        service: String,
        container: String,
        attempts: u32,
    },
    /// New container at the target spec passed its healthcheck and was
    /// promoted into routing — this is the "we did the deploy" event.
    ContainerCreated {
        service: String,
        container: String,
        spec_hash: String,
        tag: String,
    },
    /// Service was already at the desired spec — no work done. The
    /// operator can use this to verify a redeploy was a no-op.
    AlreadyAtSpec {
        service: String,
        container: String,
        spec_hash: String,
    },
    /// Old replica stopped after a successful swap. Progress.
    OldContainerStopped {
        service: String,
        container: String,
    },
    /// Old replica removed. Forensic — pairs with `ContainerCreated` to
    /// reconstruct a swap.
    ContainerRemoved {
        service: String,
        container: String,
    },
    /// Healthcheck never passed; the deploy aborted and this is the
    /// container's last log lines for triage.
    DeployFailed {
        service: String,
        container: String,
        log_tail: Vec<String>,
    },
    /// `yoink rollback <SERVICE>` started.
    RollbackStarted {
        service: String,
        target_tag: String,
    },
    /// `yoink rollback <SERVICE>` finished.
    RollbackFinished {
        service: String,
        ok: bool,
        error: Option<String>,
    },
    /// `yoink prune` removed an orphaned container.
    ContainerPruned {
        service: Option<String>,
        container: String,
    },
    /// `yoink secrets rotate` re-sealed against a new recipient.
    SecretsRotated {
        new_recipient: String,
    },
    /// File uploaded into `/var/lib/yoink/files/` for a service mount.
    FileUploaded {
        service: String,
        sha256: String,
        remote_path: String,
    },
    /// Outbound webhook attempt. Recorded for every fire, success or
    /// failure — a failed webhook never aborts the run, only this event.
    WebhookFired {
        name: String,
        ok: bool,
        status: Option<u16>,
        error: Option<String>,
    },
}

impl AuditEventKind {
    /// Forensic events bypass the ring buffer and SSH-flush
    /// immediately. Progress events buffer up and flush in batches.
    #[must_use]
    pub const fn is_forensic(&self) -> bool {
        matches!(
            self,
            AuditEventKind::RunStarted { .. }
                | AuditEventKind::RunFinished { .. }
                | AuditEventKind::LockAcquired
                | AuditEventKind::LockReleased
                | AuditEventKind::HookFinished { .. }
                | AuditEventKind::ContainerCreated { .. }
                | AuditEventKind::ContainerRemoved { .. }
                | AuditEventKind::DeployFailed { .. }
                | AuditEventKind::RollbackStarted { .. }
                | AuditEventKind::RollbackFinished { .. }
                | AuditEventKind::ContainerPruned { .. }
                | AuditEventKind::SecretsRotated { .. }
                | AuditEventKind::FileUploaded { .. }
                | AuditEventKind::WebhookFired { .. }
        )
    }
}

/// One line in the JSONL log. The envelope (timestamp, `deploy_id`,
/// actor, version) is held flat alongside the variant fields so a
/// `jq '.deploy_id' < events.jsonl` works for every event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Schema version. Bumped on backward-incompatible changes; older
    /// readers can refuse newer lines instead of misreading them.
    pub v: u32,
    /// `UUIDv7` per individual event. Stable across re-reads; lets a
    /// merge view dedupe lines that landed in both the operator log and
    /// a host log (e.g. through SSH retry double-writes), and gives the
    /// TUI a stable row identifier.
    pub event_id: String,
    /// Where this line was written: `"operator"` for the local file
    /// under `$XDG_STATE_HOME/yoink/audit/`, `"host"` for the on-host
    /// file under `~/.local/state/yoink/host-audit/`. The merge view in
    /// `yoink audit log` reads both and tags rows accordingly.
    pub origin: String,
    /// RFC 3339 wall-clock timestamp with millisecond precision.
    pub ts: String,
    /// One per `yoink up` / rollback / prune / secrets-rotate run.
    /// `UUIDv7` — time-sortable, so a lexicographic sort on this column
    /// orders events by run.
    pub deploy_id: String,
    /// `$USER@$HOSTNAME` of the operator (or `?@?` when unset, e.g. in
    /// stripped-down CI containers).
    pub actor: String,
    pub yoink_version: String,
    /// Short git SHA of the operator's working tree, if known.
    pub git_sha: Option<String>,
    /// Resolves to one of the hosts in `yoink.yaml`. Empty string for
    /// `origin: "operator"` events that aren't host-specific (e.g. a
    /// `RunStarted` that fans out to multiple hosts; the operator log
    /// records it once with `host: ""`).
    pub host: String,
    #[serde(flatten)]
    pub kind: AuditEventKind,
}

/// Per-run envelope passed into every event built by the sink. Carries
/// the fields that don't change between events (`deploy_id`, actor,
/// version, `git_sha`, command name).
#[derive(Debug, Clone)]
pub struct RunContext {
    pub deploy_id: String,
    pub actor: String,
    pub yoink_version: String,
    pub git_sha: Option<String>,
    pub command: String,
}

impl RunContext {
    /// Build a run context with a fresh `UUIDv7` `deploy_id`, the current
    /// operator, the running yoink version, and the short git SHA of
    /// `cwd` if any.
    pub fn new(command: impl Into<String>) -> Self {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "?".into());
        let host = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| hostname_via_uname().ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "?".into());
        let actor = format!("{user}@{host}");
        let git_sha = crate::git::current_short_sha(std::path::Path::new(".")).ok();
        Self {
            deploy_id: uuid::Uuid::now_v7().to_string(),
            actor,
            yoink_version: env!("CARGO_PKG_VERSION").to_string(),
            git_sha,
            command: command.into(),
        }
    }
}

fn hostname_via_uname() -> std::io::Result<String> {
    // No portable std API; fall back to `uname -n` which exists on
    // every POSIX system we deploy from.
    let out = std::process::Command::new("uname").arg("-n").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// RFC 3339 timestamp with millisecond precision. We hand-format
/// instead of pulling in `chrono` because nothing else needs it.
#[must_use]
pub fn now_rfc3339_millis() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let (y, mo, d, h, mi, s) = unix_to_components(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// RFC 3339 timestamp from a Unix-seconds value (no millisecond
/// component). Used by callers that want to compare an event's `ts`
/// string against a `since` cutoff lexicographically.
#[must_use]
pub fn ts_string_for(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_components(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

/// Civil time from Unix seconds (UTC). Shamelessly inlined to avoid a
/// chrono dep — the algorithm is Hinnant's "`days_from_civil`" inverse.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]
fn unix_to_components(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let s = secs % 86_400;
    let h = (s / 3600) as u32;
    let mi = ((s % 3600) / 60) as u32;
    let sec = (s % 60) as u32;
    let days = (secs / 86_400) as i64;
    // Hinnant: civil_from_days, with epoch at 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y_final = if mo <= 2 { y + 1 } else { y };
    (y_final as i32, mo, d, h, mi, sec)
}

/// Sink an `AuditEvent` lands in. The deploy code holds an
/// `Arc<dyn AuditSink>` — concrete impls write to a host file, an
/// in-memory ring (tests), or `/dev/null` (when audit is disabled).
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Record one event. Forensic events flush immediately; progress
    /// events buffer until `flush()`.
    async fn record(&self, event: AuditEvent);
    /// Drain any buffered progress events to durable storage.
    async fn flush(&self);
}

/// No-op sink — used when audit is intentionally disabled (no hosts in
/// the config; tests that don't care).
pub struct NullSink;

#[async_trait]
impl AuditSink for NullSink {
    async fn record(&self, _event: AuditEvent) {}
    async fn flush(&self) {}
}

/// In-memory sink for unit tests. Captures every event in order.
pub struct MemorySink {
    inner: Mutex<Vec<AuditEvent>>,
}

impl Default for MemorySink {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }
}

impl MemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn events(&self) -> Vec<AuditEvent> {
        self.inner.lock().await.clone()
    }
}

#[async_trait]
impl AuditSink for MemorySink {
    async fn record(&self, event: AuditEvent) {
        self.inner.lock().await.push(event);
    }
    async fn flush(&self) {}
}

/// Writes audit events to `~/.local/state/yoink/host-audit/events.jsonl` on each
/// host via raw SSH (same transport as `files::upload`). Forensic
/// events flush per-event; progress events buffer per-host and flush
/// in one SSH call on `flush()` or wave-end.
pub struct HostAuditSink {
    /// Host descriptors keyed by address, each with the operator-side
    /// SSH private-key path for hosts that declare `ssh_key_secret:`
    /// (`None` means the operator's normal SSH auth). Populated by
    /// `register()` before any event for that host is recorded.
    hosts: Mutex<std::collections::HashMap<String, (Host, Option<std::path::PathBuf>)>>,
    /// One pending-line buffer per host.
    buffers: Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl HostAuditSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(std::collections::HashMap::new()),
            buffers: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register a host so its buffer slot exists. Required before any
    /// event for that host can flush — without the descriptor we don't
    /// know which user/address to SSH to. `keyfile` is
    /// `DockerOps::ssh_keyfile(&host)`: without it a host that only
    /// accepts its `ssh_key_secret:` key rejects every flush.
    pub async fn register(&self, host: Host, keyfile: Option<std::path::PathBuf>) {
        self.hosts
            .lock()
            .await
            .insert(host.address.clone(), (host, keyfile));
    }

    /// Queue one serialized event line for `host_addr`.
    async fn buffer_line(&self, host_addr: &str, line: String) {
        let mut bufs = self.buffers.lock().await;
        bufs.entry(host_addr.to_string()).or_default().push(line);
    }

    /// The addresses that have a buffer slot (pending lines or not).
    async fn buffered_hosts(&self) -> Vec<String> {
        let bufs = self.buffers.lock().await;
        bufs.keys().cloned().collect()
    }

    /// Drain one host's buffer to disk via a single SSH invocation.
    /// Failure is logged to stderr; lines are dropped (deploy must not
    /// block on audit).
    async fn flush_host(&self, host_addr: &str) {
        let lines = {
            let mut bufs = self.buffers.lock().await;
            match bufs.get_mut(host_addr) {
                Some(b) if !b.is_empty() => std::mem::take(b),
                _ => return,
            }
        };
        let Some((host, keyfile)) = self.hosts.lock().await.get(host_addr).cloned() else {
            eprintln!(
                "yoink audit: no host descriptor registered for {host_addr}; \
                 dropping {} event(s)",
                lines.len()
            );
            return;
        };
        if host.is_local() {
            // Synthetic local host — no SSH; write directly to the
            // local filesystem.
            if let Err(e) = append_local(&lines).await {
                eprintln!("yoink audit: local append failed: {e}");
            }
            return;
        }
        let count = lines.len();
        if let Err(e) = append_via_ssh(&host, keyfile.as_deref(), &lines).await {
            eprintln!(
                "yoink audit: failed to flush {count} event(s) to {}: {e}",
                host.address
            );
        }
    }
}

impl Default for HostAuditSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AuditSink for HostAuditSink {
    async fn record(&self, event: AuditEvent) {
        let host_addr = event.host.clone();
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("yoink audit: serialize failed: {e}");
                return;
            }
        };
        let is_forensic = event.kind.is_forensic();
        self.buffer_line(&host_addr, line).await;
        if is_forensic {
            self.flush_host(&host_addr).await;
        }
    }

    async fn flush(&self) {
        for addr in self.buffered_hosts().await {
            self.flush_host(&addr).await;
        }
    }
}

/// Resolve the operator-side audit directory:
/// `$XDG_STATE_HOME/yoink/audit/` if set, else
/// `$HOME/.local/state/yoink/audit/`. The same lookup is done by
/// readers (`yoink audit log`) so write and read paths agree.
#[must_use]
pub fn operator_audit_dir() -> std::path::PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let mut p = std::path::PathBuf::from(state);
        p.push("yoink");
        p.push("audit");
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".local/state/yoink/audit");
        return p;
    }
    // Fallback for environments without HOME (rare; CI containers
    // sometimes strip it). Lands events under the cwd so they're at
    // least not silently dropped.
    std::path::PathBuf::from(".yoink-audit")
}

/// `<operator_audit_dir>/events.jsonl`.
#[must_use]
pub fn operator_audit_file() -> std::path::PathBuf {
    let mut p = operator_audit_dir();
    p.push("events.jsonl");
    p
}

/// Operator-side audit sink. Writes to a local JSONL file under
/// `$XDG_STATE_HOME/yoink/audit/` (or `~/.local/state/...`). All
/// writes flush per-event — local file I/O is fast enough that the
/// hybrid policy adds no value, and we want the line on disk before
/// `cmd_up` proceeds in case the operator's process gets killed.
///
/// Rotation: the active file rotates at 5 MiB to
/// `events-<UTC-stamp>.jsonl`, same shape as the on-host log.
pub struct OperatorAuditSink;

impl OperatorAuditSink {
    /// Build a sink rooted at the resolved operator audit dir. Failure
    /// to create the directory is reported lazily on the first write —
    /// audit must never block command construction.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for OperatorAuditSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AuditSink for OperatorAuditSink {
    async fn record(&self, event: AuditEvent) {
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("yoink audit: serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = append_to_operator_log(&line).await {
            eprintln!("yoink audit: operator log write failed: {e}");
        }
    }
    async fn flush(&self) {}
}

/// Append `line\n` to `operator_audit_file()`, creating the parent
/// directory if needed. Rotates when the active file exceeds
/// `ROTATE_BYTES`. Best-effort: any error is returned to the caller,
/// which warns but never aborts the run.
async fn append_to_operator_log(line: &str) -> std::io::Result<()> {
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncWriteExt;
    let dir = operator_audit_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let path = operator_audit_file();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let payload = format!("{line}\n");
    f.write_all(payload.as_bytes()).await?;
    // Audit is best-effort, but the on-host SSH path is atomic via
    // `cat >> file` in a single shell — match that durability on the
    // operator side so a SIGKILL or panic doesn't lose the tail.
    f.sync_all().await?;
    if let Ok(meta) = f.metadata().await
        && meta.len() > ROTATE_BYTES
    {
        drop(f);
        let stamp: String = now_rfc3339_millis()
            .chars()
            .filter(|c| !matches!(c, ':' | '-' | '.'))
            .collect();
        let mut rotated = dir;
        rotated.push(format!("events-{stamp}.jsonl"));
        // Failure to rotate is non-fatal — next append continues to
        // grow the active file. The operator can rotate manually.
        let _ = tokio::fs::rename(&path, &rotated).await;
    }
    Ok(())
}

/// Pipe `lines` (newline-joined) through `ssh [-i keyfile] user@host
/// sh -c "…"`.
/// Single roundtrip: mkdir + tee + size-based rotate, all in one
/// shell.
///
/// `flock -x` over a per-dir lock file serializes concurrent
/// appenders so the size-check + `mv` rotate sequence is atomic
/// across operators. Without it, two concurrent appenders could
/// both observe `sz < ROTATE_BYTES` and both skip rotation, letting
/// the file grow past the threshold for a window. fd 9 closes on
/// shell exit so the lock is always released.
///
/// `flock` ships with util-linux on every Linux distro yoink
/// targets but is absent on macOS / BSD; the `|| true` degrades to
/// the previous (unlocked) behavior on those hosts rather than
/// aborting the whole script under `set -e` and silently dropping
/// audit lines.
async fn append_via_ssh(
    host: &Host,
    keyfile: Option<&std::path::Path>,
    lines: &[String],
) -> std::io::Result<()> {
    let payload = lines.join("\n") + "\n";
    let script = append_script();
    let mut child = crate::ssh_keys::host_shell_command(host, keyfile, &script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(payload.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "ssh exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Shell script that appends stdin to the host audit file, then
/// rotates the file when it is over the size limit.
fn append_script() -> String {
    format!(
        "set -e; \
         D={HOST_AUDIT_DIR_SH}; \
         mkdir -p \"$D\"; \
         exec 9>\"$D/.lock\"; \
         flock -x 9 2>/dev/null || true; \
         cat >> \"$D/events.jsonl\"; \
         chmod 0640 \"$D/events.jsonl\" 2>/dev/null || true; \
         sz=$(wc -c < \"$D/events.jsonl\" 2>/dev/null || echo 0); \
         if [ $sz -gt {ROTATE_BYTES} ]; then \
           mv \"$D/events.jsonl\" \"$D/events-$(date -u +%Y%m%dT%H%M%SZ).jsonl\"; \
         fi"
    )
}

/// Append directly to the local filesystem (the `local` sentinel
/// host). Writes through the same path layout as remote hosts so a
/// `yoink audit log --host local` reads back consistently.
async fn append_local(lines: &[String]) -> std::io::Result<()> {
    use tokio::fs::OpenOptions;
    use tokio::io::AsyncWriteExt;
    let dir = local_host_audit_dir();
    let file = dir.join("events.jsonl");
    tokio::fs::create_dir_all(&dir).await?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .await?;
    let payload = lines.join("\n") + "\n";
    f.write_all(payload.as_bytes()).await?;
    if let Ok(meta) = f.metadata().await
        && meta.len() > ROTATE_BYTES
    {
        drop(f);
        let stamp: String = now_rfc3339_millis()
            .chars()
            .filter(|c| !matches!(c, ':' | '-' | '.'))
            .collect();
        let rotated = dir.join(format!("events-{stamp}.jsonl"));
        tokio::fs::rename(&file, &rotated).await?;
    }
    Ok(())
}

/// Build a host-origin event — destined for the on-host JSONL log.
/// Generates a fresh `event_id` and stamps `origin: "host"`. Use
/// `build_operator_event` for events that go to the local operator log.
#[must_use]
pub fn build_event(ctx: &RunContext, host: impl Into<String>, kind: AuditEventKind) -> AuditEvent {
    AuditEvent {
        v: 1,
        event_id: uuid::Uuid::now_v7().to_string(),
        origin: "host".into(),
        ts: now_rfc3339_millis(),
        deploy_id: ctx.deploy_id.clone(),
        actor: ctx.actor.clone(),
        yoink_version: ctx.yoink_version.clone(),
        git_sha: ctx.git_sha.clone(),
        host: host.into(),
        kind,
    }
}

/// Build an operator-origin event — destined for the local
/// `$XDG_STATE_HOME/yoink/audit/events.jsonl`. `host` is empty for
/// fleet-wide events (`RunStarted`, `RunFinished`); it can be set when
/// the operator log records something host-targeted but doesn't reach
/// the host (DNS failure, lock contention).
#[must_use]
pub fn build_operator_event(
    ctx: &RunContext,
    host: impl Into<String>,
    kind: AuditEventKind,
) -> AuditEvent {
    AuditEvent {
        v: 1,
        event_id: uuid::Uuid::now_v7().to_string(),
        origin: "operator".into(),
        ts: now_rfc3339_millis(),
        deploy_id: ctx.deploy_id.clone(),
        actor: ctx.actor.clone(),
        yoink_version: ctx.yoink_version.clone(),
        git_sha: ctx.git_sha.clone(),
        host: host.into(),
        kind,
    }
}

/// Convenience used by `cmd_up` and friends: wrap an `Arc<dyn AuditSink>`
/// and fire an event through it without forcing every call site to
/// build the envelope by hand.
pub async fn emit(sink: &Arc<dyn AuditSink>, ctx: &RunContext, host: &str, kind: AuditEventKind) {
    sink.record(build_event(ctx, host, kind)).await;
}

/// Build the event for a `(host, kind)` pair from [`map_deploy_event`].
/// An empty `host` means the event has no single host (pre-deploy
/// hooks: a subprocess hook runs on the operator machine, and
/// `HookFinished` carries only the hook name), so it becomes an
/// operator-origin event. Everything else is a host-origin event.
#[must_use]
pub fn build_deploy_event(
    ctx: &RunContext,
    host: impl Into<String>,
    kind: AuditEventKind,
) -> AuditEvent {
    let host = host.into();
    if host.is_empty() {
        build_operator_event(ctx, host, kind)
    } else {
        build_event(ctx, host, kind)
    }
}

/// Pick the sink an event belongs in: operator-origin events go to
/// the operator log, host-origin events to the per-host log. Sending
/// an operator-origin event (empty `host`) to [`HostAuditSink`] drops
/// it, because no host descriptor matches the empty address.
#[must_use]
pub fn sink_for<'a>(
    event: &AuditEvent,
    host_sink: &'a Arc<dyn AuditSink>,
    operator_sink: &'a Arc<dyn AuditSink>,
) -> &'a Arc<dyn AuditSink> {
    if event.origin == "operator" {
        operator_sink
    } else {
        host_sink
    }
}

/// Same as [`emit`] but for operator-origin events.
pub async fn emit_operator(
    sink: &Arc<dyn AuditSink>,
    ctx: &RunContext,
    host: &str,
    kind: AuditEventKind,
) {
    sink.record(build_operator_event(ctx, host, kind)).await;
}

/// Translate a `deploy::DeployEvent` into the host + audit kind to
/// record. Returns `None` for events we don't audit at this stage
/// (e.g. `Started`, which is a per-host announcement subsumed by the
/// later `ContainerStarted` / `ContainerCreated` events).
///
/// Hook events return an empty host. Build the event with
/// [`build_deploy_event`] and record it through [`sink_for`] so they
/// land in the operator log.
///
/// Some payload fields (`tag`, `spec_hash`) aren't carried in
/// `DeployEvent` — we derive `spec_hash` from the container name's
/// trailing short hash and leave `tag` empty for now. Callers that
/// know the tag (e.g. `cmd_up`'s `tag_overrides`) can patch it in via
/// the `tag_for` parameter, keyed by service name.
#[must_use]
#[allow(clippy::too_many_lines)] // one flat match, one arm per `DeployEvent` variant
pub fn map_deploy_event(
    service: Option<&str>,
    event: &crate::deploy::DeployEvent,
    tag_for: &dyn Fn(&str) -> String,
) -> Option<(String, AuditEventKind)> {
    use crate::deploy::DeployEvent;
    let svc = || service.unwrap_or("").to_string();
    let tag = || service.map(tag_for).unwrap_or_default();
    match event {
        DeployEvent::HookStarted { name } => Some((
            String::new(),
            AuditEventKind::HookStarted { name: name.clone() },
        )),
        DeployEvent::HookFinished { name } => Some((
            String::new(),
            AuditEventKind::HookFinished { name: name.clone() },
        )),
        DeployEvent::PullStarted {
            host,
            image,
            tag: t,
        } => Some((
            host.clone(),
            AuditEventKind::PullStarted {
                image: image.clone(),
                tag: t.clone(),
            },
        )),
        DeployEvent::NetworkReady {
            host,
            network,
            created,
        } => Some((
            host.clone(),
            AuditEventKind::NetworkReady {
                network: network.clone(),
                created: *created,
            },
        )),
        DeployEvent::ContainerStarted { host, container } => Some((
            host.clone(),
            AuditEventKind::ContainerStarted {
                service: svc(),
                container: container.clone(),
                spec_hash: short_hash_from_name(container),
                tag: tag(),
            },
        )),
        DeployEvent::HealthcheckHealthy {
            host,
            container,
            attempts,
        } => Some((
            host.clone(),
            AuditEventKind::HealthcheckHealthy {
                service: svc(),
                container: container.clone(),
                attempts: *attempts,
            },
        )),
        DeployEvent::OldContainerStopped { host, container } => Some((
            host.clone(),
            AuditEventKind::OldContainerStopped {
                service: svc(),
                container: container.clone(),
            },
        )),
        DeployEvent::AlreadyAtSpec { host, container } => Some((
            host.clone(),
            AuditEventKind::AlreadyAtSpec {
                service: svc(),
                container: container.clone(),
                spec_hash: short_hash_from_name(container),
            },
        )),
        DeployEvent::ContainerLogTail {
            host,
            container,
            lines,
        } => Some((
            host.clone(),
            AuditEventKind::DeployFailed {
                service: svc(),
                container: container.clone(),
                log_tail: lines.clone(),
            },
        )),
        // No log lines to record, but the failure itself still is:
        // the container is removed right after, so this record is all
        // that is left of it.
        DeployEvent::ContainerLogTailUnavailable {
            host,
            container,
            error,
        } => Some((
            host.clone(),
            AuditEventKind::DeployFailed {
                service: svc(),
                container: container.clone(),
                log_tail: vec![format!("(yoink: could not fetch container logs: {error})")],
            },
        )),
        DeployEvent::Done { host, container } => Some((
            host.clone(),
            AuditEventKind::ContainerCreated {
                service: svc(),
                container: container.clone(),
                spec_hash: short_hash_from_name(container),
                tag: tag(),
            },
        )),
        // Skip: `Started` (announcement-only; the rest of the wave
        // makes the same point), `PullFinished` (needs image/tag we
        // didn't keep), `HealthcheckSkipped` (low-signal).
        DeployEvent::Started { .. }
        | DeployEvent::PullFinished { .. }
        | DeployEvent::HealthcheckSkipped { .. } => None,
    }
}

/// Container names are `<service>-<short_hash>` or
/// `<service>-<short_hash>-<replica_index>`. Pick the short hash by
/// looking at the last hyphen-separated segment: if it's all digits,
/// the hash is one segment back; otherwise it's the last segment.
fn short_hash_from_name(container: &str) -> String {
    let parts: Vec<&str> = container.rsplit('-').collect();
    if parts.is_empty() {
        return String::new();
    }
    let trailing_is_index = parts[0].chars().all(|c| c.is_ascii_digit()) && !parts[0].is_empty();
    if trailing_is_index && parts.len() > 1 {
        parts[1].to_string()
    } else {
        parts[0].to_string()
    }
}

// ---------------------------------------------------------------------------
// Read path: same code is shared by the `yoink audit log|run` CLI and the TUI
// pane. Each caller layers its own filter / format on top of `fetch_events`.

/// Knobs for [`fetch_events`]. Filters that are CLI-only (service,
/// deploy-id prefix, event-name) live in `cmd_audit_log` rather than
/// here so the TUI doesn't pay for them.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Restrict the host fetch to one host's address; the operator log
    /// is still pulled (it's fleet-wide) unless `origin == Some("host")`.
    pub host_filter: Option<String>,
    /// Window relative to now, in seconds. Events older than this are
    /// dropped.
    pub since_secs: u64,
    /// Restrict to one origin. `Some("operator")` skips host fetches
    /// entirely; `Some("host")` skips the operator log.
    pub origin: Option<String>,
}

/// Result of a fetch. Errors carry the source label (host address or
/// `"operator log"`) plus a one-line message.
#[derive(Debug, Clone, Default)]
pub struct FetchOutcome {
    pub events: Vec<AuditEvent>,
    pub errors: Vec<(String, String)>,
}

/// Read the operator log + every host's log (subject to filters),
/// dedupe on `event_id`, drop events older than `since_secs`, sort
/// newest-first. Per-host fetch failures land in `errors` instead of
/// aborting the whole call.
///
/// `keypair_for` resolves the operator-side path of the SSH private
/// key for a host that declares `ssh_key_secret:` in `yoink.yaml` —
/// pass `|h| ops.ssh_keyfile(h)` from a `DockerOps` impl. Hosts that
/// don't declare a managed key get the operator's normal SSH auth
/// (agent, default identity).
pub async fn fetch_events<F>(hosts: &[Host], opts: &FetchOptions, keypair_for: F) -> FetchOutcome
where
    F: Fn(&Host) -> Option<std::path::PathBuf> + Send + Sync,
{
    let cutoff_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(opts.since_secs);
    let want_rotated = opts.since_secs > 24 * 3600;
    let cutoff_ts = ts_string_for(cutoff_unix);

    let mut events: Vec<AuditEvent> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    let want_operator = opts.origin.as_deref() != Some("host");
    if want_operator {
        match read_operator_audit_files(want_rotated).await {
            Ok(bytes) => collect_jsonl(&mut events, &mut errors, &bytes, "operator log"),
            Err(e) => errors.push(("operator log".into(), e.to_string())),
        }
    }

    let want_host = opts.origin.as_deref() != Some("operator");
    if want_host {
        let scoped: Vec<(Host, Option<std::path::PathBuf>)> = hosts
            .iter()
            .filter(|h| opts.host_filter.as_ref().is_none_or(|f| &h.address == f))
            .map(|h| (h.clone(), keypair_for(h)))
            .collect();
        let fetches = scoped.iter().map(|(h, key)| async move {
            let bytes = fetch_audit_files(h, key.as_deref(), want_rotated).await;
            (h.clone(), bytes)
        });
        let results = futures_util::future::join_all(fetches).await;
        for (host, fetch) in results {
            match fetch {
                Ok(bytes) => collect_jsonl(&mut events, &mut errors, &bytes, &host.address),
                Err(e) => errors.push((host.address, e.to_string())),
            }
        }
    }

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    events.retain(|ev| seen.insert(ev.event_id.clone()) && ev.ts >= cutoff_ts);
    events.sort_by(|a, b| b.ts.cmp(&a.ts));

    FetchOutcome { events, errors }
}

/// Read the operator-side audit files. Concatenates the active file
/// plus (when `include_rotated`) every `events-*.jsonl` in the same
/// directory. Missing files are silently treated as empty.
pub async fn read_operator_audit_files(include_rotated: bool) -> std::io::Result<String> {
    let mut out = String::new();
    if let Ok(bytes) = tokio::fs::read_to_string(operator_audit_file()).await {
        out.push_str(&bytes);
    }
    if include_rotated {
        let dir = operator_audit_dir();
        if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("events-")
                    && name_str.ends_with(".jsonl")
                    && let Ok(bytes) = tokio::fs::read_to_string(entry.path()).await
                {
                    out.push_str(&bytes);
                }
            }
        }
    }
    Ok(out)
}

/// Shell script that prints the host audit files. A missing file
/// reads as empty.
fn fetch_script(include_rotated: bool) -> String {
    // The legacy directory comes first so its older events precede
    // the current ones. A missing file reads as empty.
    if include_rotated {
        format!(
            "D={HOST_AUDIT_DIR_SH}; L={LEGACY_HOST_AUDIT_DIR}; \
             cat \"$L\"/events.jsonl \"$L\"/events-*.jsonl \
                 \"$D\"/events.jsonl \"$D\"/events-*.jsonl 2>/dev/null || true"
        )
    } else {
        format!(
            "D={HOST_AUDIT_DIR_SH}; L={LEGACY_HOST_AUDIT_DIR}; \
             cat \"$L\"/events.jsonl \"$D\"/events.jsonl 2>/dev/null || true"
        )
    }
}

/// Cat the active host audit file plus rotated files (when requested)
/// over SSH (or locally for the `local` sentinel host). Returns the
/// concatenated JSONL bytes; missing files read as empty. `key_path`
/// is the operator-side path to the SSH private key when the host
/// declares `ssh_key_secret:` (decrypted by `ssh_keys::prepare`); pass
/// `None` to fall back to the operator's normal SSH auth.
pub async fn fetch_audit_files(
    host: &Host,
    key_path: Option<&std::path::Path>,
    include_rotated: bool,
) -> anyhow::Result<String> {
    let script = fetch_script(include_rotated);
    run_audit_shell(host, key_path, &script).await
}

/// Run `script` on `host`'s shell. Local synthetic host uses `sh -c`;
/// remote uses `ssh -o BatchMode=yes [-i key_path] [-p port] user@addr script`.
/// Returns stdout.
pub async fn run_audit_shell(
    host: &Host,
    key_path: Option<&std::path::Path>,
    script: &str,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let output = if host.is_local() {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .await
            .context("local sh -c")?
    } else {
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg("-o").arg("BatchMode=yes");
        if let Some(key) = key_path {
            // -o IdentitiesOnly=yes prevents ssh from trying every key
            // in the agent first (which would land on the wrong key
            // and exhaust auth attempts before reaching `-i`).
            cmd.arg("-o").arg("IdentitiesOnly=yes").arg("-i").arg(key);
        }
        cmd.args(host.ssh_args()).arg(script);
        cmd.output()
            .await
            .with_context(|| format!("ssh {}@{}", host.user, host.address))?
    };
    if !output.status.success() {
        let prog = if host.is_local() { "sh" } else { "ssh" };
        anyhow::bail!(
            "{prog} exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a JSONL blob into `events`; malformed lines push a
/// `(source, message)` row into `errors` instead of aborting. The
/// source is the host address or `"operator log"`.
fn collect_jsonl(
    events: &mut Vec<AuditEvent>,
    errors: &mut Vec<(String, String)>,
    bytes: &str,
    source: &str,
) {
    for line in bytes.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditEvent>(trimmed) {
            Ok(ev) => events.push(ev),
            Err(e) => errors.push((source.into(), format!("malformed event: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Display helpers — used by the CLI text format and the TUI pane.

/// Stable `PascalCase` label for the variant. Used for column rendering
/// and `--event` filter matching.
#[must_use]
pub fn event_name(kind: &AuditEventKind) -> &'static str {
    use AuditEventKind as K;
    match kind {
        K::RunStarted { .. } => "RunStarted",
        K::RunFinished { .. } => "RunFinished",
        K::LockAcquired => "LockAcquired",
        K::LockReleased => "LockReleased",
        K::HookStarted { .. } => "HookStarted",
        K::HookFinished { .. } => "HookFinished",
        K::PullStarted { .. } => "PullStarted",
        K::PullFinished { .. } => "PullFinished",
        K::NetworkReady { .. } => "NetworkReady",
        K::ContainerStarted { .. } => "ContainerStarted",
        K::HealthcheckHealthy { .. } => "HealthcheckHealthy",
        K::ContainerCreated { .. } => "ContainerCreated",
        K::AlreadyAtSpec { .. } => "AlreadyAtSpec",
        K::OldContainerStopped { .. } => "OldContainerStopped",
        K::ContainerRemoved { .. } => "ContainerRemoved",
        K::DeployFailed { .. } => "DeployFailed",
        K::RollbackStarted { .. } => "RollbackStarted",
        K::RollbackFinished { .. } => "RollbackFinished",
        K::ContainerPruned { .. } => "ContainerPruned",
        K::SecretsRotated { .. } => "SecretsRotated",
        K::FileUploaded { .. } => "FileUploaded",
        K::WebhookFired { .. } => "WebhookFired",
    }
}

/// Service field on event kinds that have one; `None` for envelope
/// events (`RunStarted`, `LockAcquired`, etc.).
#[must_use]
pub fn event_service(ev: &AuditEvent) -> Option<&str> {
    use AuditEventKind as K;
    match &ev.kind {
        K::ContainerStarted { service, .. }
        | K::ContainerCreated { service, .. }
        | K::ContainerRemoved { service, .. }
        | K::AlreadyAtSpec { service, .. }
        | K::OldContainerStopped { service, .. }
        | K::DeployFailed { service, .. }
        | K::RollbackStarted { service, .. }
        | K::RollbackFinished { service, .. }
        | K::HealthcheckHealthy { service, .. }
        | K::FileUploaded { service, .. } => Some(service.as_str()),
        K::ContainerPruned { service, .. } => service.as_deref(),
        _ => None,
    }
}

/// One-line human summary of the variant payload.
#[must_use]
pub fn event_summary(kind: &AuditEventKind) -> String {
    use AuditEventKind as K;
    match kind {
        K::RunStarted { command, services } => {
            format!("{command} services=[{}]", services.join(","))
        }
        K::RunFinished {
            command, ok, error, ..
        } => {
            if *ok {
                format!("{command} ok")
            } else {
                format!("{command} FAILED: {}", error.as_deref().unwrap_or(""))
            }
        }
        K::LockAcquired | K::LockReleased => String::new(),
        K::HookStarted { name } | K::HookFinished { name } => name.clone(),
        K::PullStarted { image, tag } | K::PullFinished { image, tag } => {
            format!("{image}:{tag}")
        }
        K::NetworkReady { network, created } => {
            if *created {
                format!("{network} (created)")
            } else {
                network.clone()
            }
        }
        K::ContainerStarted {
            service,
            container,
            tag,
            ..
        }
        | K::ContainerCreated {
            service,
            container,
            tag,
            ..
        } => format!("{service} {container} tag={tag}"),
        K::AlreadyAtSpec {
            service,
            container,
            spec_hash,
        } => format!("{service} {container} spec={spec_hash}"),
        K::HealthcheckHealthy {
            service,
            container,
            attempts,
        } => format!("{service} {container} after {attempts}"),
        K::OldContainerStopped { service, container }
        | K::ContainerRemoved { service, container } => format!("{service} {container}"),
        K::DeployFailed {
            service,
            container,
            log_tail,
        } => format!("{service} {container} ({} log lines)", log_tail.len()),
        K::RollbackStarted {
            service,
            target_tag,
        } => format!("{service} → {target_tag}"),
        K::RollbackFinished { service, ok, error } => {
            if *ok {
                format!("{service} ok")
            } else {
                format!("{service} FAILED: {}", error.as_deref().unwrap_or(""))
            }
        }
        K::ContainerPruned { service, container } => {
            format!("{} {container}", service.as_deref().unwrap_or("?"))
        }
        K::SecretsRotated { new_recipient } => format!("→ {new_recipient}"),
        K::FileUploaded {
            service,
            sha256,
            remote_path,
        } => format!("{service} {sha256} → {remote_path}"),
        K::WebhookFired {
            name,
            ok,
            status,
            error,
        } => {
            if *ok {
                let s = status.map_or_else(|| "ok".to_string(), |c| format!("{c}"));
                format!("{name} {s}")
            } else {
                format!("{name} FAILED: {}", error.as_deref().unwrap_or(""))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a host audit script the way the host shell does, with
    /// `home` as the home directory of the deploy user.
    fn run_host_script(script: &str, home: &std::path::Path, stdin: &str) -> String {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .env_remove("XDG_STATE_HOME")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The host log used to live in `/var/lib/yoink/audit`, which a
    /// deploy user without root cannot create, so every flush failed
    /// with `Permission denied`. The log now lives under the home
    /// directory of that user and needs no host setup.
    #[test]
    fn host_audit_log_is_written_and_read_under_the_user_home() {
        let home = tempfile::tempdir().unwrap();
        run_host_script(&append_script(), home.path(), "{\"a\":1}\n");
        run_host_script(&append_script(), home.path(), "{\"a\":2}\n");

        let file = home
            .path()
            .join(".local/state/yoink/host-audit/events.jsonl");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        for rotated in [false, true] {
            assert_eq!(
                run_host_script(&fetch_script(rotated), home.path(), ""),
                "{\"a\":1}\n{\"a\":2}\n",
                "include_rotated={rotated}"
            );
        }
    }

    #[test]
    fn host_audit_read_on_a_host_without_a_log_is_empty() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(run_host_script(&fetch_script(true), home.path(), ""), "");
    }

    #[test]
    fn local_host_audit_dir_is_not_the_operator_log_dir() {
        let dir = local_host_audit_dir();
        assert_ne!(dir, operator_audit_dir());
        assert!(dir.ends_with("yoink/host-audit"), "{}", dir.display());
    }

    fn fixture_ctx() -> RunContext {
        RunContext {
            deploy_id: "01HFE9TESTTESTTESTTESTTEST".into(),
            actor: "alice@laptop".into(),
            yoink_version: "0.18.0".into(),
            git_sha: Some("abc1234".into()),
            command: "up".into(),
        }
    }

    #[test]
    fn jsonl_round_trip_preserves_kind() {
        let ev = build_event(
            &fixture_ctx(),
            "h1.example.com",
            AuditEventKind::ContainerCreated {
                service: "api".into(),
                container: "api-ab12cd34".into(),
                spec_hash: "ab12cd34".into(),
                tag: "sha-3fdc075".into(),
            },
        );
        let line = serde_json::to_string(&ev).unwrap();
        let back: AuditEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn build_event_stamps_host_origin_and_unique_event_id() {
        let ctx = fixture_ctx();
        let a = build_event(&ctx, "h1", AuditEventKind::LockAcquired);
        let b = build_event(&ctx, "h1", AuditEventKind::LockAcquired);
        assert_eq!(a.origin, "host");
        assert_eq!(b.origin, "host");
        assert_ne!(a.event_id, b.event_id, "every event gets a fresh id");
    }

    #[test]
    fn build_operator_event_stamps_operator_origin() {
        let ctx = fixture_ctx();
        let ev = build_operator_event(
            &ctx,
            "",
            AuditEventKind::RunStarted {
                command: "up".into(),
                services: vec!["api".into()],
            },
        );
        assert_eq!(ev.origin, "operator");
        assert_eq!(ev.host, "");
    }

    #[test]
    fn jsonl_serializes_event_field_at_top_level() {
        let ev = build_event(&fixture_ctx(), "h1", AuditEventKind::LockAcquired);
        let line = serde_json::to_string(&ev).unwrap();
        // The serde tag puts `event` at top level — required for jq
        // `.event` to work without descending into a wrapper. Same for
        // event_id and origin.
        assert!(line.contains(r#""event":"LockAcquired""#));
        assert!(line.contains(r#""deploy_id":"01HFE9"#));
        assert!(line.contains(r#""origin":"host""#));
        assert!(line.contains(r#""event_id":""#));
    }

    #[test]
    fn operator_audit_dir_ends_with_yoink_audit() {
        // Don't mutate process env (that's `unsafe` and racy across
        // parallel tests). Just confirm the resolved dir ends in the
        // expected suffix — covers the HOME and XDG_STATE_HOME branches
        // since both append `yoink/audit` and the `.yoink-audit`
        // fallback isn't a path we'd actually hit on the test machine.
        let dir = operator_audit_dir();
        let s = dir.to_string_lossy();
        assert!(
            s.ends_with("yoink/audit") || s.ends_with(".yoink-audit"),
            "unexpected dir: {s}"
        );
    }

    #[test]
    fn forensic_classification_matches_plan() {
        assert!(AuditEventKind::LockAcquired.is_forensic());
        assert!(
            AuditEventKind::ContainerCreated {
                service: "s".into(),
                container: "c".into(),
                spec_hash: "h".into(),
                tag: "t".into(),
            }
            .is_forensic()
        );
        assert!(!AuditEventKind::HookStarted { name: "n".into() }.is_forensic());
        assert!(
            !AuditEventKind::PullStarted {
                image: "i".into(),
                tag: "t".into(),
            }
            .is_forensic()
        );
        assert!(
            !AuditEventKind::HealthcheckHealthy {
                service: "s".into(),
                container: "c".into(),
                attempts: 1,
            }
            .is_forensic()
        );
    }

    #[tokio::test]
    async fn memory_sink_records_in_order() {
        let sink = MemorySink::new();
        let ctx = fixture_ctx();
        sink.record(build_event(&ctx, "h1", AuditEventKind::LockAcquired))
            .await;
        sink.record(build_event(&ctx, "h1", AuditEventKind::LockReleased))
            .await;
        let events = sink.events().await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, AuditEventKind::LockAcquired));
        assert!(matches!(events[1].kind, AuditEventKind::LockReleased));
    }

    #[tokio::test]
    async fn host_sink_buffers_progress_until_flush() {
        // Without a registered host, flush silently drops buffered
        // events and the test verifies that record() doesn't panic
        // and that the buffer accumulates.
        let sink = HostAuditSink::new();
        let ctx = fixture_ctx();
        sink.record(build_event(
            &ctx,
            "h1",
            AuditEventKind::PullStarted {
                image: "alpine".into(),
                tag: "3.20".into(),
            },
        ))
        .await;
        sink.record(build_event(
            &ctx,
            "h1",
            AuditEventKind::PullFinished {
                image: "alpine".into(),
                tag: "3.20".into(),
            },
        ))
        .await;
        let buf = sink.buffers.lock().await;
        assert_eq!(buf.get("h1").map(Vec::len), Some(2));
    }

    #[test]
    fn short_hash_from_name_handles_replicas() {
        assert_eq!(short_hash_from_name("api-ab12cd34"), "ab12cd34");
        assert_eq!(short_hash_from_name("api-ab12cd34-0"), "ab12cd34");
        assert_eq!(short_hash_from_name("api-ab12cd34-15"), "ab12cd34");
        assert_eq!(short_hash_from_name("nodash"), "nodash");
        assert_eq!(short_hash_from_name(""), "");
    }

    #[test]
    fn map_deploy_event_translates_done_to_container_created() {
        use crate::deploy::DeployEvent;
        let ev = DeployEvent::Done {
            host: "h1".into(),
            container: "api-ab12cd34".into(),
        };
        let (host, kind) = map_deploy_event(Some("api"), &ev, &|_| "v1".into()).unwrap();
        assert_eq!(host, "h1");
        match kind {
            AuditEventKind::ContainerCreated {
                service,
                container,
                spec_hash,
                tag,
            } => {
                assert_eq!(service, "api");
                assert_eq!(container, "api-ab12cd34");
                assert_eq!(spec_hash, "ab12cd34");
                assert_eq!(tag, "v1");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn map_deploy_event_records_deploy_failed_when_logs_are_unavailable() {
        use crate::deploy::DeployEvent;
        let ev = DeployEvent::ContainerLogTailUnavailable {
            host: "h1".into(),
            container: "api-ab12cd34".into(),
            error: "ssh session closed".into(),
        };
        let (host, kind) = map_deploy_event(Some("api"), &ev, &|_| "v1".into()).unwrap();
        assert_eq!(host, "h1");
        let AuditEventKind::DeployFailed {
            service,
            container,
            log_tail,
        } = kind
        else {
            panic!("unexpected kind: {kind:?}");
        };
        assert_eq!(service, "api");
        assert_eq!(container, "api-ab12cd34");
        assert_eq!(
            log_tail,
            vec!["(yoink: could not fetch container logs: ssh session closed)".to_string()]
        );
    }

    /// Hook events carry no host. Before the fix they were built as
    /// host-origin events with an empty host and `HostAuditSink`
    /// dropped them ("no host descriptor registered").
    #[tokio::test]
    async fn hook_events_route_to_the_operator_sink() {
        use crate::deploy::DeployEvent;
        let ctx = fixture_ctx();
        let host_mem = Arc::new(MemorySink::new());
        let operator_mem = Arc::new(MemorySink::new());
        let host_sink: Arc<dyn AuditSink> = host_mem.clone();
        let operator_sink: Arc<dyn AuditSink> = operator_mem.clone();

        let events = [
            DeployEvent::HookStarted {
                name: "migrate".into(),
            },
            DeployEvent::HookFinished {
                name: "migrate".into(),
            },
            DeployEvent::ContainerStarted {
                host: "h1".into(),
                container: "api-abcdef12".into(),
            },
        ];
        for event in &events {
            let (host, kind) =
                map_deploy_event(Some("api"), event, &|_| String::new()).expect("audited");
            let ev = build_deploy_event(&ctx, host, kind);
            sink_for(&ev, &host_sink, &operator_sink).record(ev).await;
        }

        let operator_events = operator_mem.events().await;
        assert_eq!(
            operator_events
                .iter()
                .map(|e| event_name(&e.kind))
                .collect::<Vec<_>>(),
            vec!["HookStarted", "HookFinished"]
        );
        assert!(operator_events.iter().all(|e| e.origin == "operator"));
        let host_events = host_mem.events().await;
        assert_eq!(host_events.len(), 1);
        assert_eq!(host_events[0].origin, "host");
        assert_eq!(host_events[0].host, "h1");
    }

    #[test]
    fn map_deploy_event_skips_low_signal_variants() {
        use crate::deploy::DeployEvent;
        let started = DeployEvent::Started {
            service: "api".into(),
            tag: "v1".into(),
            host: "h1".into(),
        };
        assert!(map_deploy_event(Some("api"), &started, &|_| String::new()).is_none());
    }

    #[test]
    fn now_rfc3339_millis_has_correct_shape() {
        let s = now_rfc3339_millis();
        assert_eq!(s.len(), 24);
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
        assert!(s.ends_with('Z'));
    }

    #[test]
    #[allow(clippy::many_single_char_names)] // mirrors the variable names in unix_to_components
    fn unix_to_components_known_value() {
        // 1_777_999_321 → 2026-05-05T16:42:01Z. Sanity-check Hinnant.
        let t = 1_777_999_321_u64;
        let (y, mo, d, h, mi, s) = unix_to_components(t);
        assert_eq!((y, mo, d, h, mi, s), (2026, 5, 5, 16, 42, 1));
        // Epoch itself: 1970-01-01T00:00:00Z.
        let (y, mo, d, h, mi, s) = unix_to_components(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }
}
