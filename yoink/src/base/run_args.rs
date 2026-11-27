//! The text forms docker takes for `run:` option values: memory and
//! cpu limits, `--device`, `--publish`, tmpfs options and bind modes.
//! Pure parsers into bollard's model types. Config validation and the
//! container-spec builder in `docker` share them, so the two
//! cannot disagree on what a value means.

use std::collections::HashMap;

use bollard::models::PortBinding;
use thiserror::Error;

/// Validate a single `--device` spec without producing the bollard
/// `DeviceMapping`. Same grammar as [`parse_device`]; used by
/// `Config::validate()` so malformed entries are caught at config-load
/// time without dragging bollard's types into `config`.
pub(crate) fn validate_device_spec(spec: &str) -> Result<(), BuildError> {
    parse_device(spec).map(|_| ())
}

/// Parse a single `--device` spec — `<host-path>[:<container-path>[:<perms>]]`,
/// matching docker's CLI. Defaults: container path == host path; perms `rwm`.
///
/// Validates: both paths must begin with `/`; perms (when present) must be
/// non-empty, drawn from `{r, w, m}`, with no duplicates.
pub(crate) fn parse_device(spec: &str) -> Result<bollard::models::DeviceMapping, BuildError> {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    let bad = || BuildError::Device(spec.to_string());

    let host = *parts.first().ok_or_else(bad)?;
    if host.is_empty() || !host.starts_with('/') {
        return Err(bad());
    }

    let container = match parts.get(1).copied() {
        None => host,
        Some(c) if c.starts_with('/') => c,
        _ => return Err(bad()),
    };

    let perms = match parts.get(2).copied() {
        None => "rwm",
        Some(p) => {
            if p.is_empty() || p.len() > 3 {
                return Err(bad());
            }
            let mut seen = [false; 3];
            for ch in p.chars() {
                let idx = match ch {
                    'r' => 0,
                    'w' => 1,
                    'm' => 2,
                    _ => return Err(bad()),
                };
                if seen[idx] {
                    return Err(bad());
                }
                seen[idx] = true;
            }
            p
        }
    };

    Ok(bollard::models::DeviceMapping {
        path_on_host: Some(host.to_string()),
        path_in_container: Some(container.to_string()),
        cgroup_permissions: Some(perms.to_string()),
    })
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid memory value {0:?}: expected like \"512m\", \"512Mi\", or \"1Gi\"")]
    Memory(String),
    #[error(
        "invalid cpus value {0:?}: expected an absolute core count like \"2\", \"1.5\", or \"500m\""
    )]
    Cpus(String),
    #[error(
        "invalid device spec {0:?}: expected \"<host-path>[:<container-path>[:<perms>]]\" \
         with absolute paths and `<perms>` drawn from `r`, `w`, `m`"
    )]
    Device(String),
    #[error(
        "invalid publish spec {0:?}: expected \"host_port:container_port[/proto]\" or \"ip:host_port:container_port[/proto]\""
    )]
    Publish(String),
}

/// The smallest memory limit the Docker daemon accepts on Linux
/// (`linuxMinMemory`, 6 MiB). `0` is also accepted: it means no limit.
const DOCKER_MIN_MEMORY: i64 = 6 * 1_024 * 1_024;

/// Parse a docker-style memory value: bare bytes (`536870912`), or with
/// suffix `b`, `k`, `m`, `g` (case-insensitive). Returns the value in
/// bytes as `i64` (bollard's type). Rejects what the Docker daemon
/// rejects: a negative value, or a limit above `0` and below 6 MiB.
/// `0` stays valid (no limit).
pub fn parse_memory(s: &str) -> Result<i64, BuildError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(BuildError::Memory(s.to_string()));
    }
    // K8s-style binary suffixes (Ki/Mi/Gi/Ti) are checked first so a
    // bare `m` or `M` doesn't shadow them. Docker-style single-letter
    // suffixes follow.
    let (num, mult) = if let Some(rest) = strip_suffix_ci(s, "ki") {
        (rest, 1_024_i64)
    } else if let Some(rest) = strip_suffix_ci(s, "mi") {
        (rest, 1_024_i64.pow(2))
    } else if let Some(rest) = strip_suffix_ci(s, "gi") {
        (rest, 1_024_i64.pow(3))
    } else if let Some(rest) = strip_suffix_ci(s, "ti") {
        (rest, 1_024_i64.pow(4))
    } else {
        match s.as_bytes().last() {
            Some(b'b' | b'B') => (&s[..s.len() - 1], 1_i64),
            Some(b'k' | b'K') => (&s[..s.len() - 1], 1_024_i64),
            Some(b'm' | b'M') => (&s[..s.len() - 1], 1_024 * 1_024),
            Some(b'g' | b'G') => (&s[..s.len() - 1], 1_024 * 1_024 * 1_024),
            Some(c) if c.is_ascii_digit() => (s, 1_i64),
            _ => return Err(BuildError::Memory(s.to_string())),
        }
    };
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| BuildError::Memory(s.to_string()))?;
    let bytes = n.saturating_mul(mult);
    if bytes < 0 || (bytes > 0 && bytes < DOCKER_MIN_MEMORY) {
        return Err(BuildError::Memory(s.to_string()));
    }
    Ok(bytes)
}

/// Case-insensitive `strip_suffix` that returns the prefix when the
/// suffix matches at the tail (case-folded ASCII compare). Used by
/// the memory parser so `Mi`, `MI`, and `mi` all mean the same thing.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() < suffix.len() {
        return None;
    }
    let (head, tail) = s.split_at(s.len() - suffix.len());
    if tail.eq_ignore_ascii_case(suffix) {
        Some(head)
    } else {
        None
    }
}

/// Parse a k8s-style CPU string into nano-CPUs (the unit
/// `bollard::HostConfig::nano_cpus` expects). 1 core = 1e9.
///
/// Accepted forms:
///   - `"2"` / `"1.5"` — bare number, absolute cores' worth.
///   - `"500m"` — millicores (k8s convention; 1000m = 1 core).
///
/// Non-positive values are rejected so the operator's intent isn't
/// silently turned into "uncapped" by the daemon.
pub fn parse_cpus(s: &str) -> Result<i64, BuildError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(BuildError::Cpus(s.to_string()));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let nano = if let Some(rest) = s.strip_suffix('m').or_else(|| s.strip_suffix('M')) {
        let millis: f64 = rest
            .trim()
            .parse()
            .map_err(|_| BuildError::Cpus(s.to_string()))?;
        (millis * 1_000_000.0) as i64
    } else {
        let cores: f64 = s.parse().map_err(|_| BuildError::Cpus(s.to_string()))?;
        (cores * 1_000_000_000.0) as i64
    };
    if nano <= 0 {
        return Err(BuildError::Cpus(s.to_string()));
    }
    Ok(nano)
}

/// Append `noexec`, `nosuid`, and `nodev` to a tmpfs option string
/// unless the operator explicitly opts out by including the positive
/// form (`exec`, `suid`, `dev`). Existing options are preserved
/// verbatim. The hardening blocks the "drop a binary on writable
/// scratch + exec it" escape chain — a tmpfs is a great place for an
/// attacker with code execution to land a payload, and `noexec`
/// renders that payload unrunnable; `nosuid` prevents setuid
/// elevation off the same surface; `nodev` blocks creating device
/// nodes.
#[must_use]
pub fn harden_tmpfs_options(opts: &str) -> String {
    let parts: Vec<&str> = opts
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let has = |needle: &str| parts.iter().any(|p| p.eq_ignore_ascii_case(needle));
    let mut out: Vec<String> = parts.iter().map(|s| (*s).to_string()).collect();
    for (positive, negative) in [("exec", "noexec"), ("suid", "nosuid"), ("dev", "nodev")] {
        if !has(positive) && !has(negative) {
            out.push(negative.to_string());
        }
    }
    out.join(",")
}

/// Append `:ro` to a bind-mount string unless it already specifies a
/// mode (`ro` or `rw` as a token of the third colon-delimited field,
/// e.g. `:rw,Z`). Operator opts in to writable bind mounts with
/// explicit `:rw`. Removes the "I bind-mounted a config dir and the
/// app started scribbling on /etc on the host" footgun.
///
/// Config load rejects a `run.binds` entry whose third field has no
/// mode (see [`bind_mode_missing`]). A `files:` entry can still reach
/// here with options and no mode (`local:/etc/app.conf:Z`), because a
/// `files:` mount is always read-only. For that case `ro` is added in
/// front of the options: `src:dst:Z` becomes `src:dst:ro,Z`.
#[must_use]
pub fn default_bind_to_ro(bind: &str) -> String {
    // A bind has the shape "src:dst" or "src:dst:mode[,opt,opt]".
    // Docker accepts the comma-separated options in any order.
    match bind_mode_missing(bind) {
        Some(opts) if !opts.is_empty() => {
            let src_dst = &bind[..bind.len() - opts.len() - 1];
            format!("{src_dst}:ro,{opts}")
        }
        Some(_) => bind.to_string(),
        None if bind.matches(':').count() >= 2 => bind.to_string(),
        None => format!("{bind}:ro"),
    }
}

/// The third field of `bind` when it is present but holds neither
/// `ro` nor `rw`: options only (an `SELinux` label like `Z`,
/// propagation, `nocopy`), or nothing at all (`src:dst:`). Docker
/// mounts such a bind read-write, so config load rejects it.
/// `None` for `src:dst` and for a third field with a mode.
#[must_use]
pub fn bind_mode_missing(bind: &str) -> Option<&str> {
    let mut parts = bind.splitn(3, ':');
    parts.next()?;
    parts.next()?;
    let opts = parts.next()?;
    if opts.split(',').any(|o| o == "ro" || o == "rw") {
        return None;
    }
    Some(opts)
}

/// Parse docker-cli `--publish` strings into a port-bindings map keyed
/// by `"<container_port>/<proto>"` (the format bollard expects). Accepts:
///   - `"8080:80"`
///   - `"8080:80/udp"`
///   - `"127.0.0.1:8080:80"` (host-ip pin)
///   - `"127.0.0.1:8080:80/tcp"`
///
/// Multiple specs targeting the same container port stack into the
/// returned `Vec<PortBinding>` (Docker supports binding the same
/// container port to multiple host ips).
pub(crate) fn parse_port_bindings(
    publish: &[String],
) -> Result<HashMap<String, Option<Vec<PortBinding>>>, BuildError> {
    let mut out: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for spec in publish {
        let (mapping, proto) = match spec.split_once('/') {
            Some((m, p)) if !p.is_empty() => (m, p),
            Some(_) => return Err(BuildError::Publish(spec.clone())),
            None => (spec.as_str(), "tcp"),
        };
        let parts: Vec<&str> = mapping.split(':').collect();
        let (host_ip, host_port, container_port) = match parts.as_slice() {
            [hp, cp] => (None, *hp, *cp),
            [ip, hp, cp] => (Some((*ip).to_string()), *hp, *cp),
            _ => return Err(BuildError::Publish(spec.clone())),
        };
        if host_port.is_empty()
            || container_port.is_empty()
            || host_port.parse::<u16>().is_err()
            || container_port.parse::<u16>().is_err()
        {
            return Err(BuildError::Publish(spec.clone()));
        }
        let key = format!("{container_port}/{proto}");
        let binding = PortBinding {
            host_ip,
            host_port: Some(host_port.to_string()),
        };
        out.entry(key)
            .or_insert_with(|| Some(Vec::new()))
            .as_mut()
            .expect("just inserted Some")
            .push(binding);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_memory_accepts_common_suffixes() {
        assert_eq!(parse_memory("8388608").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_memory("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory("8192k").unwrap(), 8192 * 1024);
    }

    #[test]
    fn parse_memory_accepts_k8s_suffixes() {
        assert_eq!(parse_memory("512Mi").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("1Gi").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory("8192Ki").unwrap(), 8192 * 1024);
        // Case-insensitive
        assert_eq!(parse_memory("2gi").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_cpus_accepts_bare_and_millicores() {
        assert_eq!(parse_cpus("2").unwrap(), 2_000_000_000);
        assert_eq!(parse_cpus("1.5").unwrap(), 1_500_000_000);
        assert_eq!(parse_cpus("0.5").unwrap(), 500_000_000);
        assert_eq!(parse_cpus("500m").unwrap(), 500_000_000);
        assert_eq!(parse_cpus("1500m").unwrap(), 1_500_000_000);
    }

    #[test]
    fn harden_tmpfs_appends_safe_flags_when_missing() {
        // Operator-provided options preserved + the three negatives appended.
        let out = harden_tmpfs_options("size=64m,mode=1777");
        // Order is preserved (operator's first, ours appended).
        let parts: std::collections::BTreeSet<&str> = out.split(',').collect();
        assert!(parts.contains("size=64m"));
        assert!(parts.contains("mode=1777"));
        assert!(parts.contains("noexec"));
        assert!(parts.contains("nosuid"));
        assert!(parts.contains("nodev"));
    }

    #[test]
    fn harden_tmpfs_respects_explicit_opt_outs() {
        // If the operator specifically wants exec/suid/dev, we don't
        // override them. Each negative is independent.
        let out = harden_tmpfs_options("size=64m,exec");
        assert!(out.contains("exec"));
        assert!(!out.contains("noexec"));
        assert!(out.contains("nosuid"));
        assert!(out.contains("nodev"));
    }

    #[test]
    fn harden_tmpfs_idempotent_on_already_hardened_string() {
        let out = harden_tmpfs_options("size=64m,noexec,nosuid,nodev");
        // Hardening a string that already has the negatives shouldn't
        // duplicate them.
        assert_eq!(out.matches("noexec").count(), 1);
        assert_eq!(out.matches("nosuid").count(), 1);
        assert_eq!(out.matches("nodev").count(), 1);
    }

    #[test]
    fn default_bind_appends_ro_when_no_mode_set() {
        assert_eq!(
            default_bind_to_ro("/host/path:/container/path"),
            "/host/path:/container/path:ro"
        );
    }

    #[test]
    fn default_bind_leaves_explicit_modes_alone() {
        assert_eq!(
            default_bind_to_ro("/host:/container:ro"),
            "/host:/container:ro"
        );
        assert_eq!(
            default_bind_to_ro("/host:/container:rw"),
            "/host:/container:rw"
        );
        // Multi-option mode strings (SELinux Z, etc.) also count as
        // explicit operator intent.
        assert_eq!(
            default_bind_to_ro("/host:/container:rw,Z"),
            "/host:/container:rw,Z"
        );
    }

    /// Regression: a bind with only an `SELinux` label (`:Z`) counted as
    /// a chosen mode and was mounted read-write.
    #[test]
    fn default_bind_to_ro_adds_ro_in_front_of_options_without_a_mode() {
        assert_eq!(
            default_bind_to_ro("/etc/app:/etc/app:Z"),
            "/etc/app:/etc/app:ro,Z"
        );
        assert_eq!(default_bind_to_ro("/h:/c:z,rshared"), "/h:/c:ro,z,rshared");
        assert_eq!(default_bind_to_ro("/h:/c:Z,ro"), "/h:/c:Z,ro");
        assert_eq!(default_bind_to_ro("/h:/c:ro"), "/h:/c:ro");
    }

    #[test]
    fn bind_mode_missing_for_options_or_an_empty_field_without_ro_or_rw() {
        assert_eq!(bind_mode_missing("/h:/c:Z"), Some("Z"));
        assert_eq!(bind_mode_missing("/h:/c:z,rshared"), Some("z,rshared"));
        assert_eq!(bind_mode_missing("/h:/c:"), Some(""));
        assert_eq!(bind_mode_missing("/h:/c"), None);
        assert_eq!(bind_mode_missing("/h:/c:ro"), None);
        assert_eq!(bind_mode_missing("/h:/c:rw,Z"), None);
    }

    #[test]
    fn parse_cpus_rejects_non_positive_and_garbage() {
        assert!(parse_cpus("0").is_err());
        assert!(parse_cpus("-1").is_err());
        assert!(parse_cpus("0m").is_err());
        assert!(parse_cpus("abc").is_err());
        assert!(parse_cpus("").is_err());
    }

    #[test]
    fn parse_memory_rejects_garbage() {
        assert!(parse_memory("").is_err());
        assert!(parse_memory("abc").is_err());
        assert!(parse_memory("12x").is_err());
    }

    /// Regression: negative values and limits below Docker's 6 MiB
    /// minimum parsed, and the daemon rejected them at create. `0`
    /// means no limit in Docker and stays valid.
    #[test]
    fn parse_memory_rejects_what_docker_rejects() {
        assert!(parse_memory("-512m").is_err());
        assert!(parse_memory("-1").is_err());
        assert!(parse_memory("1024").is_err());
        assert!(parse_memory("5m").is_err());
        assert_eq!(parse_memory("0").unwrap(), 0);
        assert_eq!(parse_memory("6m").unwrap(), 6 * 1024 * 1024);
    }

    #[test]
    fn parse_device_one_field_defaults_container_path_and_perms() {
        let d = parse_device("/dev/fuse").unwrap();
        assert_eq!(d.path_on_host.as_deref(), Some("/dev/fuse"));
        assert_eq!(d.path_in_container.as_deref(), Some("/dev/fuse"));
        assert_eq!(d.cgroup_permissions.as_deref(), Some("rwm"));
    }

    #[test]
    fn parse_device_two_fields_remaps_container_path() {
        let d = parse_device("/dev/sdb:/dev/disk").unwrap();
        assert_eq!(d.path_on_host.as_deref(), Some("/dev/sdb"));
        assert_eq!(d.path_in_container.as_deref(), Some("/dev/disk"));
        assert_eq!(d.cgroup_permissions.as_deref(), Some("rwm"));
    }

    #[test]
    fn parse_device_three_fields_honors_perms() {
        let d = parse_device("/dev/null:/dev/null:r").unwrap();
        assert_eq!(d.cgroup_permissions.as_deref(), Some("r"));

        let d = parse_device("/dev/x:/dev/x:wm").unwrap();
        assert_eq!(d.cgroup_permissions.as_deref(), Some("wm"));
    }

    #[test]
    fn parse_device_rejects_relative_paths() {
        assert!(matches!(
            parse_device("dev/fuse"),
            Err(BuildError::Device(_))
        ));
        assert!(matches!(
            parse_device("/dev/fuse:dev/x"),
            Err(BuildError::Device(_))
        ));
    }

    #[test]
    fn parse_device_rejects_bad_perms() {
        // Empty perms (trailing colon).
        assert!(matches!(
            parse_device("/dev/fuse:/dev/fuse:"),
            Err(BuildError::Device(_))
        ));
        // Unknown char.
        assert!(matches!(
            parse_device("/dev/fuse:/dev/fuse:rwx"),
            Err(BuildError::Device(_))
        ));
        // Duplicates.
        assert!(matches!(
            parse_device("/dev/fuse:/dev/fuse:rrw"),
            Err(BuildError::Device(_))
        ));
        // Too long.
        assert!(matches!(
            parse_device("/dev/fuse:/dev/fuse:rwmw"),
            Err(BuildError::Device(_))
        ));
    }
}
