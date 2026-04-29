//! Multi-service yoink config. One TOML file describes the full set of
//! services to run on a fleet of hosts, plus optional pre-deploy hooks.
//! The unified model — every service goes through the same reconcile
//! loop; the differences are config knobs (healthcheck or not, ports
//! published or not, etc.) rather than service kinds.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] yaml_serde::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("invalid include glob {pattern:?}: {source}")]
    GlobPattern {
        pattern: String,
        #[source]
        source: glob::PatternError,
    },
    #[error("error walking include glob {pattern:?}: {source}")]
    GlobWalk {
        pattern: String,
        #[source]
        source: glob::GlobError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Optional banner text rendered in the TUI's top chrome on every
    /// view. Use it to mark a config — typically `"PRODUCTION — TREAD
    /// CAREFULLY"` — so an operator can't miss what they're pointed at.
    /// Free-form: anything goes, but keep it short (the chrome reserves
    /// a single line).
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub deploy: DeployDefaults,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub secrets: Option<SecretsConfig>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    /// Optional registry credentials. When set, yoink resolves the named
    /// secrets from the configured `secrets:` provider and passes them
    /// to docker as `X-Registry-Auth` on every pull. Without this, only
    /// public registries (or hosts with cached `docker login` creds)
    /// will pull successfully.
    #[serde(default)]
    pub registry: Option<RegistryConfig>,
    /// Optional reverse-proxy config. The proxy is auto-enabled when
    /// any service declares `domain:`; this block only needs to be set
    /// when overriding defaults (e.g. to set `email:` for ACME or to
    /// pin a custom Caddy image with plugins). See [`ProxyConfig`].
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    /// Glob patterns (relative to this file's directory) of additional
    /// config fragments to load. Each fragment may declare `services`
    /// and/or `hooks.pre_deploy`; everything else (`hosts`, `secrets`,
    /// `deploy.network`) stays in the main file. Lets you put one
    /// service per file under `services/*.yaml` while keeping a single
    /// shared network and host fleet.
    #[serde(default)]
    pub include: Vec<String>,
    /// Directory the config was loaded from. Used as the base for
    /// resolving relative paths (e.g. `service.run.files` entries).
    /// Skipped during serde — populated only by `load_from_path`.
    #[serde(default, skip)]
    pub config_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub hooks: HookConfig,
}

/// Defaults that apply across every service. A service can override most
/// of these in its own block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployDefaults {
    /// Docker networks the deploy declares. Every name a service
    /// references in its own `networks:` list must appear here.
    /// At least one entry required.
    pub networks: Vec<String>,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub on_failure: OnFailure,
}

impl Default for DeployDefaults {
    fn default() -> Self {
        Self {
            networks: vec![default_network()],
            strategy: Strategy::default(),
            on_failure: OnFailure::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    #[default]
    Parallel,
    Rolling,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    #[default]
    Halt,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub address: String,
    pub user: String,
    /// Optional: name of a sealed-secret entry holding an SSH private
    /// key (PEM format) to use when connecting to this host. When set,
    /// yoink loads the key into a per-process `ssh-agent` at deploy
    /// time, sets `SSH_AUTH_SOCK`, and bollard's spawned ssh client
    /// inherits the env. Lets you ship the deploy key with the repo
    /// (encrypted at rest in `secrets.age`) without requiring every
    /// operator to add the key to their personal ssh-agent.
    /// Use this when the operator's host doesn't already have key-based
    /// auth set up — fresh VPS with `root` user, throwaway hosts, etc.
    #[serde(default)]
    pub ssh_key_secret: Option<String>,
}

/// Reverse-proxy config. The proxy itself is a yoink-managed Caddy
/// service (`name = "yoink-proxy"`, `kind = ServiceKind::Proxy`) that's
/// synthesized at config-load time when any service has `domain:` set.
/// This block tunes the synthesized service. All fields optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Force the proxy on or off, regardless of whether any service
    /// has `domain:`. Default: implicit `true` when any service
    /// declares `domain:`, else `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Email address for Let's Encrypt ACME registration. Required
    /// when any service uses `tls: auto` (which is the default for
    /// services with `domain:`). Unused when `proxy.tls.cert_secret`
    /// is set — yoink doesn't ACME on top of an inline cert.
    #[serde(default)]
    pub email: Option<String>,
    /// Caddy image. Override to use an `xcaddy`-built image with
    /// plugins (e.g. `caddy-storage-redis` for multi-host certs,
    /// `caddy-ratelimit`, `caddy-l4`). Default `caddy:2`.
    #[serde(default)]
    pub image: Option<String>,
    /// Named Docker volume for ACME state, certs, and OCSP staples.
    /// Persisted across proxy restarts. Default `yoink_caddy_data`.
    #[serde(default)]
    pub cert_volume: Option<String>,
    /// Host IP to bind `:80` and `:443` to. Default unset → bind
    /// to all interfaces (`0.0.0.0`). Common use: bind to a
    /// Tailscale IP so the proxy is reachable only over the
    /// tailnet (`bind: 100.10.0.1`), or to `127.0.0.1` for local
    /// dev where the proxy fronts a tunneled cloudflared session.
    #[serde(default)]
    pub bind: Option<String>,
    /// Proxy-level TLS configuration. When set, every routed service
    /// inherits this cert (and optional client-auth) by default.
    /// Per-service `tls_cert_secret:` / `tls_key_secret:` overrides
    /// for the rare different-cert-per-service case.
    ///
    /// The common shape (one wildcard cert covers everything):
    /// ```yaml
    /// proxy:
    ///   tls:
    ///     cert_secret: CF_ORIGIN_CERT
    ///     key_secret:  CF_ORIGIN_KEY
    ///     client_auth:
    ///       mode: require_and_verify
    ///       trust_pool_secret: CF_ORIGIN_PULL_CA
    /// ```
    #[serde(default)]
    pub tls: Option<ProxyTls>,
    /// Compile a custom caddy binary on each proxy host using xcaddy,
    /// baking in the listed plugins (rate-limit, l4, redis-storage,
    /// caddy-dns/*, etc.). Mutually exclusive with `image:` — pick one.
    /// See `XcaddyConfig` for shape.
    #[serde(default)]
    pub xcaddy: Option<XcaddyConfig>,
    /// Top-level Caddy JSON config snippet, deep-merged into the
    /// rendered config before `/load`. Escape hatch for global
    /// settings yoink doesn't model as typed fields:
    /// - `apps.http.servers.srv0.trusted_proxies` /
    ///   `client_ip_headers` to read the real client IP from
    ///   `CF-Connecting-IP` (paired with the
    ///   `WeidiDeng/caddy-cloudflare-ip` plugin).
    /// - `storage` for shared ACME state (e.g. `caddy-storage-redis`).
    /// - `apps.cache` for `caddy-storage-redis`-backed
    ///   `cache-handler` configuration.
    /// - `apps.crowdsec` / `apps.coraza` global app blocks.
    ///
    /// The string is parsed as JSON; merge is recursive on objects
    /// (user-supplied keys win on conflict, base values are preserved
    /// at non-overlapping keys). Validation rejects non-object JSON.
    #[serde(default)]
    pub config_extra: Option<String>,
    /// Caddy handlers to run for **every** request before any
    /// service-specific route matches. Yoink wraps the per-service
    /// routes in a `subroute` handler so the global handlers form a
    /// single shared middleware chain — the natural place for
    /// proxy-wide concerns:
    ///
    /// - CrowdSec bouncer (deny based on IP decisions before the
    ///   request hits app-level handlers).
    /// - Coraza WAF / OWASP CRS (signature-based payload inspection
    ///   on every request, not just the ones a particular service
    ///   opts into).
    /// - Global rate limiting that should apply to all hostnames.
    ///
    /// Each entry is a JSON snippet: a single handler object
    /// (`{"handler": "crowdsec", ...}`), a route object (`{"match":
    /// ..., "handle": ...}` — same as `caddy_extra_json`'s convenience
    /// shape), or an array mixing the two. Validated as JSON at
    /// config-load time; Caddy itself catches semantic errors at
    /// `/load`.
    #[serde(default)]
    pub global_handlers: Vec<String>,
    /// Same shape as `global_handlers:`, but the listed handlers run
    /// **after** the matched service route's handlers (and after any
    /// per-service `caddy_extra_json:`). Use this slot when a global
    /// concern needs to *follow* per-service logic — e.g. a fleet-wide
    /// audit-log handler that runs after per-service auth has tagged
    /// the request, or a global response-rewrite that runs after the
    /// upstream has produced its response.
    ///
    /// Wire ordering inside the wrapper route is `pre_handlers...,
    /// subroute(per-service routes), post_handlers...`. Yoink wraps
    /// once when either list is non-empty.
    #[serde(default)]
    pub global_handlers_after: Vec<String>,
}

/// Build inputs for an on-host xcaddy compile. The resulting image is
/// tagged locally as `yoink-caddy:<short-hash>` where the hash is
/// derived from these fields, so identical inputs across runs short-
/// circuit via `image_present` and skip the rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XcaddyConfig {
    /// Caddy modules to bake in. Bare module path (`github.com/foo/bar`)
    /// or pinned (`github.com/foo/bar@v1.2.3`) — same syntax as
    /// `xcaddy build --with`. Sorted alphabetically before hashing /
    /// rendering so order in the config file doesn't change the tag.
    pub plugins: Vec<String>,
    /// Caddy version to compile (positional arg to `xcaddy build`).
    /// Default `2` — matches the upstream `caddy:2` floating tag.
    #[serde(default)]
    pub caddy_version: Option<String>,
    /// Runtime image used as the second stage of the build (`FROM ...`
    /// at the bottom of the Dockerfile). Default `caddy:2`.
    #[serde(default)]
    pub base_image: Option<String>,
    /// Builder image carrying xcaddy + Go toolchain. Default
    /// `caddy:2-builder`. Pin to `caddy:<ver>-builder` to also pin the
    /// xcaddy CLI version.
    #[serde(default)]
    pub builder_image: Option<String>,
    /// Environment variables exported into the builder stage before
    /// `xcaddy build` runs. Useful for `GOPRIVATE`, `GOPROXY`,
    /// `GOSUMDB`, `NETRC` overrides when fetching private Go modules.
    /// Sorted by key before hashing so map insertion order doesn't
    /// change the resulting image tag.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Go-module replace directives forwarded to xcaddy as `--replace
    /// <entry>`. Each entry is the raw form xcaddy expects (e.g.
    /// `github.com/foo/bar=github.com/me/bar-fork@v1.0.0`). Useful for
    /// pinning a transitive dependency or running a forked plugin
    /// before upstream merges your fix. Sorted alphabetically before
    /// hashing / rendering.
    #[serde(default)]
    pub replace: Vec<String>,
}

/// Proxy-level TLS. When `cert_secret` + `key_secret` are set, ACME
/// is implicitly off and a `:80 → :443` redirect is auto-emitted by
/// the renderer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyTls {
    /// Sealed-secret name holding the PEM cert (full chain). Inlined
    /// into the rendered Caddy JSON at deploy time. When set, every
    /// routed service uses this cert unless it overrides via
    /// per-service `tls_cert_secret:`.
    pub cert_secret: String,
    /// Sealed-secret name holding the PEM private key matching
    /// `cert_secret`.
    pub key_secret: String,
    /// Optional client-certificate authentication (mTLS). Required
    /// for setups like Cloudflare origin-pull where the proxy
    /// rejects requests that don't present a Cloudflare-signed
    /// client cert.
    #[serde(default)]
    pub client_auth: Option<ClientAuth>,
}

/// Client-certificate auth (mTLS) for proxy-level TLS. The trust
/// pool is sealed-secret content, inlined into rendered JSON — no
/// host-filesystem dependency.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAuth {
    /// Caddy `client_authentication.mode`. Default
    /// `require_and_verify` (the strict choice; `require` skips
    /// CA verification, `verify_if_given` is opt-in).
    #[serde(default = "default_client_auth_mode")]
    pub mode: ClientAuthMode,
    /// Sealed-secret name holding the trust-pool CA chain (PEM).
    /// Common case: Cloudflare's origin-pull CA bundle.
    pub trust_pool_secret: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthMode {
    Request,
    Require,
    VerifyIfGiven,
    #[default]
    RequireAndVerify,
}

impl ClientAuthMode {
    /// String form Caddy expects in JSON
    /// (`client_authentication.mode`).
    #[must_use]
    pub fn as_caddy(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Require => "require",
            Self::VerifyIfGiven => "verify_if_given",
            Self::RequireAndVerify => "require_and_verify",
        }
    }
}

fn default_client_auth_mode() -> ClientAuthMode {
    ClientAuthMode::RequireAndVerify
}

/// Default proxy image when neither `proxy.image:` nor `proxy.xcaddy:`
/// is set, and the default runtime stage of an xcaddy build. Floating
/// upstream tag — bumps automatically when caddy ships a new release.
pub const CADDY_DEFAULT_IMAGE: &str = "caddy:2";

/// Default xcaddy builder image (carries the xcaddy CLI + Go toolchain).
pub const CADDY_DEFAULT_BUILDER_IMAGE: &str = "caddy:2-builder";

impl ProxyConfig {
    /// Resolve the Caddy image, applying the default. When `xcaddy:` is
    /// set, returns the content-addressed local tag the builder will
    /// produce (`yoink-caddy:<hash>`); otherwise the user's `image:`
    /// override or `caddy:2`.
    #[must_use]
    pub fn resolved_image(&self) -> String {
        if let Some(x) = &self.xcaddy {
            return x.resolved_local_tag();
        }
        self.image
            .clone()
            .unwrap_or_else(|| CADDY_DEFAULT_IMAGE.to_string())
    }

    /// Resolve the cert volume name, applying the default.
    #[must_use]
    pub fn resolved_cert_volume(&self) -> String {
        self.cert_volume
            .clone()
            .unwrap_or_else(|| "yoink_caddy_data".to_string())
    }
}

impl XcaddyConfig {
    #[must_use]
    pub fn resolved_base_image(&self) -> &str {
        self.base_image.as_deref().unwrap_or(CADDY_DEFAULT_IMAGE)
    }

    #[must_use]
    pub fn resolved_builder_image(&self) -> &str {
        self.builder_image
            .as_deref()
            .unwrap_or(CADDY_DEFAULT_BUILDER_IMAGE)
    }

    /// Plugins, alphabetized. Both the rendered Dockerfile and the
    /// `hash()` input use this so order in the config file is irrelevant.
    #[must_use]
    pub fn sorted_plugins(&self) -> Vec<String> {
        let mut v = self.plugins.clone();
        v.sort();
        v
    }

    /// `replace` entries, alphabetized. Same rationale as `sorted_plugins`.
    #[must_use]
    pub fn sorted_replace(&self) -> Vec<String> {
        let mut v = self.replace.clone();
        v.sort();
        v
    }

    /// 16-hex-char content hash over every field that affects the
    /// resulting binary: `(caddy_version, base_image, builder_image,
    /// sorted_plugins, sorted_env, sorted_replace)`. Inputs are joined
    /// with `\n` separators so distinct field orderings can't alias.
    /// `caddy_version` hashes as the empty string when unset, matching
    /// the Dockerfile renderer (which omits the positional arg →
    /// xcaddy uses caddy's latest tagged release).
    #[must_use]
    pub fn hash(&self) -> String {
        let mut input = String::new();
        input.push_str(self.caddy_version.as_deref().unwrap_or(""));
        input.push('\n');
        input.push_str(self.resolved_base_image());
        input.push('\n');
        input.push_str(self.resolved_builder_image());
        input.push('\n');
        for p in self.sorted_plugins() {
            input.push_str(&p);
            input.push('\n');
        }
        // `BTreeMap` iteration is already key-sorted, so the encoding
        // is stable without an extra sort step.
        for (k, v) in &self.env {
            input.push_str(k);
            input.push('=');
            input.push_str(v);
            input.push('\n');
        }
        for r in self.sorted_replace() {
            input.push_str(&r);
            input.push('\n');
        }
        crate::deploy::short_sha256(&input)
    }

    /// `yoink-caddy:<hash>` — the local tag produced by
    /// `proxy::xcaddy::ensure_xcaddy_image` and consumed by
    /// `ProxyConfig::resolved_image`.
    #[must_use]
    pub fn resolved_local_tag(&self) -> String {
        format!("yoink-caddy:{}", self.hash())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// Hostname of the registry (e.g. `4db05qgnlk.registry.depot.dev`).
    pub server: String,
    /// Secret key that holds the registry username. Resolved against
    /// the configured `secrets:` provider at deploy time.
    pub username_secret: String,
    /// Secret key that holds the registry password / API token.
    pub password_secret: String,
}

/// Secrets provider config. The default, batteries-included shape is
/// `age` — a single sealed file (`secrets.age` next to `yoink.yaml`)
/// committed to the repo, decrypted at deploy time with one key
/// `age` is the batteries-included path: a single `secrets.age` file
/// in the repo, decrypted with one identity sourced from
/// `YOINK_AGE_KEY` / `YOINK_AGE_KEY_FILE` / `~/.config/yoink/age.key`
/// (the operator typically pipes the private key into a third-party
/// secret store and exposes it as an env var at deploy time).
///
/// `command` is the bring-your-own-tool path: yoink runs the
/// configured command and reads the resulting bundle from stdout.
/// Auto-detects between dotenv (`KEY=value\n`) and JSON
/// (`{"K":"V"}`) based on the first non-whitespace byte. Lets
/// operators wire up 1Password (`op inject`), Doppler (`doppler
/// secrets download --format env`), `HashiCorp` Vault (`vault kv
/// get -format=json`), AWS Secrets Manager, the Infisical CLI, etc.
/// without yoink growing first-party integrations for each.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase", deny_unknown_fields)]
pub enum SecretsConfig {
    /// Sealed dotenv file, encrypted with [age](https://age-encryption.org).
    Age {
        /// Path to the sealed file relative to the config file's
        /// directory. Defaults to `secrets.age` when unset.
        #[serde(default)]
        file: Option<String>,
        /// Recipients to seal *new* writes against (used by
        /// `yoink secrets edit / seal`). Each entry is an age public
        /// key (`age1...`). Decryption only needs one matching identity.
        #[serde(default)]
        recipients: Vec<String>,
    },
    /// External CLI provider. yoink invokes `command`, captures
    /// stdout, and parses it as a secrets bundle. Format is
    /// auto-detected unless explicitly set.
    Command {
        /// Argv to spawn. First element is the binary, the rest are
        /// arguments. Spawned without a shell, so users can't (and
        /// shouldn't) embed pipes or env-var expansion in a string —
        /// quote-and-split is on the operator.
        command: Vec<String>,
        /// Override the bundle format. `auto` (default) inspects the
        /// first non-whitespace byte: `{` → JSON, anything else →
        /// dotenv. Set explicitly when the auto-detect could trip on
        /// values that legitimately start with `{`.
        #[serde(default)]
        format: SecretsFormat,
    },
}

/// On-the-wire bundle shape from a `provider: command`. `Auto` (the
/// default) inspects the first non-whitespace byte of stdout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretsFormat {
    #[default]
    Auto,
    Dotenv,
    Json,
}

/// Local build instructions for a service. Powers `yoink build`
/// (and the `yoink up --no-registry` "deploy without a registry" path).
/// Resolves all paths relative to the loaded config file's directory
/// — same convention as `service.run.files` and `include:` globs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// Build context directory passed to `docker build`. Default `.`.
    #[serde(default = "default_build_context")]
    pub context: String,
    /// Path to the Dockerfile relative to `context`. Default
    /// `Dockerfile` (docker's own default).
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// `--build-arg KEY=VALUE` pairs.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    /// `--target` for multi-stage builds.
    #[serde(default)]
    pub target: Option<String>,
    /// Extra `docker build` flags to forward verbatim, inserted
    /// before the context arg. Escape hatch for flags yoink doesn't
    /// model directly (e.g. `--platform`, `--secret`, `--ssh`).
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_build_context() -> String {
    ".".to_string()
}

fn default_hsts() -> bool {
    true
}

/// Discriminator for special-purpose services. Most services are
/// regular app workloads (`None`); the only variant today is `Proxy`
/// for the implicit `_proxy` service yoink synthesizes when any
/// service declares `domain:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Proxy,
}

/// `domain:` accepts either a single hostname or a list — a service
/// can be reachable on multiple domain names (e.g. `api.example.com`
/// + `api.example.net`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DomainSpec {
    Single(String),
    Many(Vec<String>),
}

impl DomainSpec {
    /// Flatten to a sorted list of hostnames. Used by the renderer.
    #[must_use]
    pub fn as_list(&self) -> Vec<String> {
        match self {
            Self::Single(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

/// TLS mode for a service's `domain:`. Default `Auto` (Let's Encrypt
/// via ACME). `Off` serves on `:80` only. `Cert` uses an inline
/// certificate sourced from `tls_cert_secret` + `tls_key_secret` in
/// the sealed-secrets bundle (e.g. Cloudflare Origin Certificates).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    #[default]
    Auto,
    Off,
    Cert,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub image: String,
    /// Human-readable summary of what this service does. Written to every
    /// container as `org.opencontainers.image.description` (the OCI standard
    /// label key) so the TUI can surface it for both yoink-deployed and
    /// 3rd-party OCI-compliant containers via a single label lookup.
    #[serde(default)]
    pub description: Option<String>,
    /// Special-purpose service marker. Set automatically on the
    /// implicit `_proxy` service; users do not write this.
    #[serde(default)]
    pub kind: Option<ServiceKind>,
    /// Local-build instructions. When set, `yoink build [<service>]`
    /// runs `docker build` against this context, tagging the result
    /// as `<image>:<tag>`. Pairs with `yoink up --no-registry` for the
    /// "edit Dockerfile, deploy directly to host" loop.
    #[serde(default)]
    pub build: Option<BuildConfig>,
    /// Image tag, OR a content digest `sha256:<hex>` for digest-pinned
    /// deploys. Optional: when omitted from the config the operator
    /// MUST pass `--tag <name>=<value>` (or per-service `--service x
    /// --tag <value>`) at deploy time. Useful for code-versioned
    /// services (api, web) where the right answer is "whatever CI
    /// just built" and a hard-coded tag in the file rots immediately.
    /// Stable infrastructure (caddy, redis, etc.) keeps a literal
    /// tag here so `task yoink:up` works without arguments.
    ///
    /// Operators may also write the digest inline as
    /// `image: repo@sha256:<hex>`; `Config::load_from_path` normalizes
    /// that into separate `image` (repo) + `tag` (`sha256:<hex>`)
    /// fields so the runtime never sees the combined form.
    #[serde(default)]
    pub tag: Option<String>,
    /// Restrict to a subset of `[[hosts]]`. None = every configured host.
    #[serde(default)]
    pub hosts: Option<Vec<String>>,
    /// Literal env vars baked into the container.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of secret keys to fetch from the configured `[secrets]`
    /// provider and inject as env vars at deploy time. Each entry is
    /// the same string for the secret key and the env var name.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Inject a secret under a *different* env var name. Map of
    /// `env_var_name -> secret_key`. Useful when the upstream secret
    /// store names a value one way (e.g. `AUTH_DATABASE_MIGRATE_URL`)
    /// but the running container expects another (e.g. `AUTH_DATABASE_URL`).
    #[serde(default)]
    pub env_from_secrets: BTreeMap<String, String>,
    /// Extra labels to apply to the container (in addition to the yoink
    /// management labels).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Service-scoped pre-deploy hooks. Fire just before this service
    /// is reconciled, regardless of how many hosts it runs on.
    /// Filtering by `--service` skips both the service and its hooks
    /// (so `yoink up --service api` does NOT run web's migrations).
    #[serde(default)]
    pub pre_deploy: Vec<HookSpec>,
    /// Other services this one needs deployed FIRST. Drives the per-
    /// service deploy order in `yoink up` (a topological sort over the
    /// graph). Most useful with per-service tier networks: a caller
    /// has to wait until the callee is on its shared network before
    /// its container can resolve the callee's name. Empty = no
    /// ordering constraint. Names must reference declared services;
    /// cycles are rejected at config load.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Docker networks this service attaches to. When unset (the
    /// common single-network case), the service attaches to every
    /// network declared under `deploy.networks`. When set, must be
    /// a non-empty subset of `deploy.networks` — this is how the
    /// operator opts a service out of a network for tier
    /// isolation (e.g. caddy with `[frontend]` only, no db reach).
    #[serde(default)]
    pub networks: Option<Vec<String>>,
    /// Hostname(s) the reverse proxy should route to this service.
    /// Setting `domain:` on any service auto-enables the bundled Caddy
    /// proxy. The proxy gets the routes for `domain:` → `<container>:<run.port>`.
    /// Requires `run.port:` to be set.
    #[serde(default)]
    pub domain: Option<DomainSpec>,
    /// TLS mode for `domain:`. `auto` uses Let's Encrypt via ACME
    /// (the default); `off` serves on `:80` only; `cert` uses an
    /// inline certificate from `tls_cert_secret` + `tls_key_secret`.
    #[serde(default)]
    pub tls: TlsMode,
    /// Name of the sealed-secret entry holding a PEM certificate
    /// (full chain). Used when `tls: cert`. Pairs with
    /// `tls_key_secret`. Common case: Cloudflare Origin Certificates
    /// rotated quarterly into `secrets.age`.
    #[serde(default)]
    pub tls_cert_secret: Option<String>,
    /// Name of the sealed-secret entry holding the PEM private key
    /// matching `tls_cert_secret`. Used when `tls: cert`.
    #[serde(default)]
    pub tls_key_secret: Option<String>,
    /// Raw JSON merged into this service's Caddy site-block route as
    /// additional `handle` entries (rendered before the auto-generated
    /// `reverse_proxy` handler). Escape hatch for Caddy features yoink
    /// doesn't model — `forward_auth`, `rate_limit`, `headers`, etc.
    /// Accepts either a list of handlers (`[{"handler": "...", ...}]`)
    /// or a list of routes (`[{"match": ..., "handle": ...}]`); routes
    /// are auto-wrapped in a `subroute` handler so the operator never
    /// has to know about `subroute`. Yoink does not validate the
    /// contents — invalid handlers will fail at Caddy's `/load` step
    /// with the API's parse error surfaced verbatim.
    /// See <https://caddyserver.com/docs/json/apps/http/servers/routes/handle/>.
    #[serde(default)]
    pub caddy_extra_json: Option<String>,
    /// Like `caddy_extra_json:` but takes Caddyfile syntax — the
    /// friendlier shape Caddy's docs and ecosystem use:
    ///
    /// ```yaml
    /// caddy_extra_caddyfile: |
    ///   forward_auth authelia:9091 {
    ///     uri /api/verify?rd=https://auth.example.com
    ///     copy_headers Remote-User Remote-Groups Remote-Email
    ///   }
    /// ```
    ///
    /// Yoink shells out to `caddy adapt --adapter caddyfile` (in an
    /// ephemeral container) at render time to convert the snippet to
    /// JSON, then splices the resulting handlers into the route.
    /// Requires docker on the operator's machine. Cannot be combined
    /// with `caddy_extra_json:` on the same service — pick one shape.
    #[serde(default)]
    pub caddy_extra_caddyfile: Option<String>,
    /// Talk to the backend over HTTP/2 cleartext (`h2c`). Renders a
    /// `transport: { protocol: http, versions: [h2c] }` block on the
    /// auto-generated `reverse_proxy` handler. Required for native
    /// gRPC backends (Tonic, grpc-go, grpc-java) and for HTTP/2-only
    /// upstream apps. Doesn't affect what Caddy serves to clients —
    /// just how it dials the upstream.
    #[serde(default)]
    pub upstream_h2c: bool,
    /// One of the entries in `domain:` to treat as canonical. Yoink
    /// auto-renders a 308 redirect from every other entry to this one.
    /// Common pattern: `domain: [example.com, www.example.com]` +
    /// `canonical_domain: example.com` redirects www → apex.
    /// Must match exactly one of the strings in `domain:`.
    #[serde(default)]
    pub canonical_domain: Option<String>,
    /// Compress responses with gzip + zstd. Renders Caddy's `encode`
    /// handler before `reverse_proxy`. No-op when behind Cloudflare
    /// (the edge already compresses); useful for direct-origin
    /// deploys without a CDN in front.
    #[serde(default)]
    pub compression: bool,
    /// Emit the `Strict-Transport-Security` header on every response.
    /// **Default `true` for TLS sites** (`tls: auto` or `tls: cert`,
    /// or proxy-level inheritance). Universal best practice once
    /// you've committed to HTTPS — every modern browser pins the
    /// site to HTTPS-only after the first visit. Set to `false` for
    /// the rare case where you serve mixed HTTP/HTTPS or are
    /// migrating off the TLS path.
    #[serde(default = "default_hsts")]
    pub hsts: bool,
    /// Path-prefix routing. When set, this service's route only
    /// matches requests where the URL path starts with this prefix
    /// (Caddy `path` matcher; `*` is allowed at the end). Lets two
    /// services share a hostname:
    ///
    /// ```yaml
    /// services:
    ///   - name: api
    ///     domain: example.com
    ///     path_prefix: /api/*
    ///     run: { port: 8080 }
    ///   - name: web
    ///     domain: example.com   # same host
    ///     run: { port: 3000 }
    /// ```
    ///
    /// Yoink orders routes so path-constrained ones come before
    /// catch-all ones — first match wins, as Caddy does.
    #[serde(default)]
    pub path_prefix: Option<String>,
    pub run: ServiceRun,
}

impl ServiceConfig {
    /// The networks this service will actually attach to — its
    /// declared list when set, else every network in `deploy.networks`.
    #[must_use]
    pub fn effective_networks(&self, deploy: &DeployDefaults) -> Vec<String> {
        self.networks
            .clone()
            .unwrap_or_else(|| deploy.networks.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRun {
    /// Application port the container listens on. Required when
    /// `healthcheck_path` is set (yoink uses it to build the probe URL).
    #[serde(default)]
    pub port: Option<u16>,
    /// HTTP path probed to confirm the new container is healthy. None
    /// disables the healthcheck — the swap goes straight from start to
    /// stop-old.
    #[serde(default)]
    pub healthcheck_path: Option<String>,
    #[serde(default = "default_healthcheck_timeout", with = "humantime_serde")]
    pub healthcheck_timeout: Duration,
    /// Number of container replicas to run per host for this service.
    /// Default 1. Replicas share the same network alias so the embedded
    /// docker DNS returns multiple A records — caddy (or any other
    /// in-network client) sees them as a round-robin upstream pool.
    /// Replicas are mutually exclusive with `publish:` because host
    /// port bindings are exclusive at the kernel level.
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default = "default_drain_timeout", with = "humantime_serde")]
    pub drain_timeout: Duration,

    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Vec<String>,

    #[serde(default)]
    pub options: RunOptions,
    /// docker-cli style port publishes: `"443:443"`, `"127.0.0.1:5050:80"`,
    /// `"8080:8080/tcp"`. Parsed at deploy time.
    #[serde(default)]
    pub publish: Vec<String>,
    /// docker-cli style bind mounts: `"/root/certs:/certs:ro"`,
    /// `"./config/Caddyfile:/etc/caddy/Caddyfile:ro"`. Host paths must
    /// already exist on the target host.
    #[serde(default)]
    pub binds: Vec<String>,
    /// docker-cli style named-volume mounts: `"caddy-data:/data"`.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Files to upload from the operator's machine to the target host
    /// before `docker create`, then bind-mount into the container.
    /// Format: `"local_path:container_path[:ro]"`.
    /// Local paths are resolved relative to the config file. The on-host
    /// staging path is content-addressed (a sha256 of the file) so
    /// concurrent deploys never overwrite each other and a config edit
    /// triggers a redeploy via the spec hash.
    #[serde(default)]
    pub files: Vec<String>,
}

impl Default for ServiceRun {
    fn default() -> Self {
        Self {
            port: None,
            healthcheck_path: None,
            healthcheck_timeout: default_healthcheck_timeout(),
            replicas: default_replicas(),
            drain_timeout: default_drain_timeout(),
            entrypoint: None,
            cmd: Vec::new(),
            options: RunOptions::default(),
            publish: Vec::new(),
            binds: Vec::new(),
            volumes: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// Per-container runtime options. **All security-relevant fields
/// default to a hardened profile** — drop every Linux capability,
/// disallow setuid escalation, immutable rootfs. Operators who need
/// looser settings opt out explicitly:
///
/// ```yaml
/// run:
///   cap_drop: []                  # opt out of capability drop
///   security_opt: []              # opt out of no-new-privileges
///   read_only: false              # opt out of immutable rootfs
/// ```
///
/// `cpus` + `pids_limit` are uncapped by default; both are sane
/// knobs to bound a runaway container without tuning the whole host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    #[serde(default)]
    pub network_aliases: Vec<String>,
    /// Memory limit. K8s-style strings accepted: `"512Mi"`, `"1Gi"`,
    /// `"512m"`, `"1g"`. Lowercase suffixes are binary
    /// (`m` = `Mi` = 2²⁰); k8s `Ki`/`Mi`/`Gi` are also binary;
    /// uppercase `K`/`M`/`G` are binary too (matches docker, not k8s
    /// — k8s `K`/`M`/`G` are decimal but we follow docker convention).
    /// `None` → uncapped.
    #[serde(default)]
    pub memory: Option<String>,
    /// CPU limit, k8s-style. Absolute cores' worth of CPU time —
    /// **not** a fraction of host CPU. `"2"` on a 32-core box still
    /// caps this container at 2 cores. Suffixes: `"500m"` =
    /// 500 millicores = ½ core; bare numbers like `"1.5"` are also
    /// accepted. `None` → uncapped. Translates to docker's
    /// `nano_cpus` (1 core = 1e9 ns/sec).
    #[serde(default)]
    pub cpus: Option<String>,
    /// Maximum number of pids/threads the container can create.
    /// **Default: `1024`** — bounds the fork-bomb class of exploits
    /// while staying well above what most workloads need (Rust
    /// async, Node, Redis, Caddy all stay under ~50). JVM apps with
    /// large thread pools may need to bump this; set to `None` for
    /// unlimited.
    #[serde(default = "default_pids_limit")]
    pub pids_limit: Option<i64>,
    /// Linux capabilities to drop. **Default: `["ALL"]`** — every
    /// privileged operation is blocked unless re-enabled via
    /// `cap_add`. Set to `[]` to opt out (only when you know you need
    /// the full default capability set).
    #[serde(default = "default_cap_drop")]
    pub cap_drop: Vec<String>,
    /// Linux capabilities to add back after `cap_drop`. Common
    /// example: `[NET_BIND_SERVICE]` for a reverse proxy that needs
    /// :80/:443.
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Container security options. **Default:
    /// `["no-new-privileges:true"]`** — blocks setuid binaries inside
    /// the container from gaining new caps via execve. Set to `[]`
    /// to opt out (almost never needed).
    #[serde(default = "default_security_opt")]
    pub security_opt: Vec<String>,
    /// Mount the rootfs read-only. **Default: `true`** — combine
    /// with `tmpfs:` for any directories the app needs to write to.
    /// Set to `false` to opt out for images that scribble all over
    /// their rootfs at runtime (legacy webapps, the pgadmin image,
    /// etc.).
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    /// In-memory mount points (path → mount options). Pairs with
    /// `read_only: true` to give the app *some* writable space
    /// without giving up immutability of the rootfs.
    #[serde(default)]
    pub tmpfs: BTreeMap<String, String>,
    #[serde(default)]
    pub restart: Option<String>,
    /// Override the container's effective user.
    /// **Default: `"65534:65534"` (nobody:nogroup)** — yoink runs every
    /// new container as a non-root unprivileged uid so a process
    /// breakout doesn't immediately give the attacker file-system
    /// access through the few mounts (tmpfs, binds) the hardened
    /// profile leaves writable. Override with `"0:0"` for images that
    /// genuinely need root (host-metrics collectors, otel reading
    /// `/proc`), or with a different non-root uid if 65534 collides
    /// with the image's pre-baked file ownership.
    #[serde(default = "default_user")]
    pub user: Option<String>,
    /// Run `tini` as PID 1 (docker's `--init`). **Default: `true`.**
    /// Most app images run their language runtime as PID 1, which
    /// doesn't reap zombies and often eats SIGTERM instead of
    /// forwarding it to children — drains stall, healthcheck-gated
    /// rolls hang, zombie procs accumulate. tini fixes both. Set to
    /// `false` for images that already ship their own init system
    /// (systemd-in-containers, s6-overlay, supervisord, …).
    #[serde(default = "default_init")]
    pub init: bool,
}

fn default_cap_drop() -> Vec<String> {
    vec!["ALL".to_string()]
}

fn default_security_opt() -> Vec<String> {
    vec!["no-new-privileges:true".to_string()]
}

fn default_read_only() -> bool {
    true
}

fn default_init() -> bool {
    true
}

/// Default `user:` value for runtime containers. `nobody:nogroup` on
/// most distros (alpine, debian-slim, ubuntu, distroless). Operators
/// who need root (otel, host-metrics) opt out via `user: "0:0"`;
/// images with pre-baked file ownership for a specific uid override
/// to that uid (`"redis"`, `"1000:1000"`, etc.).
#[allow(clippy::unnecessary_wraps)]
fn default_user() -> Option<String> {
    Some("65534:65534".to_string())
}

// Wrapped in `Option<i64>` because that's what the field expects;
// clippy's `unnecessary_wraps` lint is wrong here.
#[allow(clippy::unnecessary_wraps)]
fn default_pids_limit() -> Option<i64> {
    Some(1024)
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            network_aliases: Vec::new(),
            memory: None,
            cpus: None,
            pids_limit: default_pids_limit(),
            cap_drop: default_cap_drop(),
            cap_add: Vec::new(),
            security_opt: default_security_opt(),
            read_only: default_read_only(),
            tmpfs: BTreeMap::new(),
            restart: None,
            user: default_user(),
            init: default_init(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    #[serde(default)]
    pub pre_deploy: Vec<HookSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    pub name: String,
    pub image: String,
    /// Either a literal tag string or `{ service = "api" }` to mirror
    /// the deploy-time tag of another service in this config.
    pub tag: HookTag,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Same shape as on `ServiceConfig` — inject a secret value under
    /// a renamed env var. Lets a hook reuse a Kamal-era secret (e.g.
    /// `AUTH_DATABASE_MIGRATE_URL`) under the env name the container
    /// actually reads (`AUTH_DATABASE_URL`).
    #[serde(default)]
    pub env_from_secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum HookTag {
    Literal(String),
    Ref { service: String },
}

fn default_healthcheck_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_drain_timeout() -> Duration {
    Duration::from_secs(15)
}

fn default_network() -> String {
    "yoink".to_string()
}

fn default_replicas() -> u32 {
    1
}

/// Subset of `Config` permitted in fragment files included via the
/// main config's `include:` globs. Fragments may add services and
/// pre-deploy hooks; they cannot redefine `hosts`, `deploy.network`,
/// or `secrets` — those are global and live in the main file.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFragment {
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub hooks: HookConfig,
}

/// True when one of the platform-default docker socket paths exists.
/// Doesn't actually try to connect — that's a config-load fast path
/// and hangs would be much worse than a stale check.
fn local_socket_exists() -> bool {
    // Order matters: rootless / Docker Desktop / OrbStack on macOS
    // expose `~/.docker/run/docker.sock`; the system daemon symlinks
    // the same path at `/var/run/docker.sock`. Either one is good.
    if std::path::Path::new("/var/run/docker.sock").exists() {
        return true;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".docker/run/docker.sock");
        if p.exists() {
            return true;
        }
    }
    false
}

impl Config {
    /// Walk `self.services` filtered by an optional name list. `None`
    /// returns every service in topo order; `Some(&[…])` keeps only
    /// the named ones (in topo order, not in the order the operator
    /// listed them — `yoink up --service web --service api` still
    /// runs api first when api → … → web in the dep graph).
    /// Used wherever the CLI takes `--service`.
    pub fn selected_services<'a>(
        &'a self,
        filter: Option<&'a [String]>,
    ) -> impl Iterator<Item = &'a ServiceConfig> {
        self.services
            .iter()
            .filter(move |svc| filter.is_none_or(|names| names.iter().any(|n| n == &svc.name)))
    }

    /// True iff at least one service requires a `docker pull` at deploy
    /// time (i.e., its image is not built locally). When false, every
    /// service has a `build:` block — there is literally nothing to
    /// pull, so the registry-pull phase + auth is dead weight.
    ///
    /// This is the predicate that drives auto `--no-registry`: when
    /// false, plain `yoink up` behaves as if the operator had passed
    /// `--no-registry`.
    #[must_use]
    pub fn any_service_requires_pull(&self) -> bool {
        self.services.iter().any(|svc| svc.build.is_none())
    }

    /// Iterate the non-build services whose `image:` host matches the
    /// configured registry. Empty when no `registry:` is set, no
    /// service uses it, or every using service has a `build:` block.
    /// Drives both `requires_configured_registry()` and the doctor's
    /// orphan-registry check.
    pub fn services_using_configured_registry(&self) -> impl Iterator<Item = &ServiceConfig> {
        let server = self.registry.as_ref().map(|r| r.server.as_str());
        self.services.iter().filter(move |svc| {
            let Some(server) = server else { return false };
            svc.build.is_none()
                && crate::docker::image_registry_host(&svc.image)
                    .is_some_and(|h| h.eq_ignore_ascii_case(server))
        })
    }

    /// True iff some non-build service's image is hosted on the
    /// configured registry, i.e. the configured creds will actually be
    /// used at deploy time. Used by `yoink doctor` to flag orphan
    /// `registry:` blocks; not used to drive auto `--no-registry`
    /// (that's `any_service_requires_pull`, which is intentionally
    /// narrower — Docker Hub library images like `postgres` should
    /// still be host-pulled, not unregistry-shipped).
    #[must_use]
    pub fn requires_configured_registry(&self) -> bool {
        self.services_using_configured_registry().next().is_some()
    }

    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        // Parse the main file *without* validation — fragments contribute
        // services + hooks, so duplicate / dangling-reference checks only
        // make sense after the merge.
        let mut cfg: Self = yaml_serde::from_str(&text)?;
        cfg.config_dir = path.parent().map(std::path::Path::to_path_buf);
        cfg.merge_includes()?;
        cfg.normalize_image_references()?;
        crate::proxy::inject_implicit_proxy(&mut cfg)?;
        cfg.validate()?;
        cfg.topo_sort_services()?;
        Ok(cfg)
    }

    /// Like [`load_from_path`] but skips the final `validate()` pass —
    /// returns even when the loaded config has dangling `depends_on`
    /// or other cross-service references.
    ///
    /// Used by `yoink add`: the operator may have referenced a service
    /// that doesn't exist yet (e.g. `depends_on: [postgres]` in the
    /// app, before `yoink add postgres` ran). Hard-rejecting at load
    /// time would lock them out of the very command that fixes the
    /// reference. The add flow validates each *rendered fragment* on
    /// its own (`Config::parse_str` inside the orchestrator), and the
    /// post-add reload (when the operator opts into `--up`) goes
    /// through the strict path.
    pub fn load_from_path_relaxed(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut cfg: Self = yaml_serde::from_str(&text)?;
        cfg.config_dir = path.parent().map(std::path::Path::to_path_buf);
        cfg.merge_includes()?;
        cfg.normalize_image_references()?;
        // Skip inject_implicit_proxy + validate + topo_sort — those
        // are what reject incomplete configs. The add flow only reads
        // `secrets`, `services`, and `include`; an un-injected proxy
        // is fine because we're not deploying.
        Ok(cfg)
    }

    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = yaml_serde::from_str(text)?;
        config.normalize_image_references()?;
        crate::proxy::inject_implicit_proxy(&mut config)?;
        config.validate()?;
        config.topo_sort_services()?;
        Ok(config)
    }

    /// Split inline `image: repo@sha256:<hex>` into separate
    /// `image: repo` + `tag: sha256:<hex>` so the runtime always sees
    /// the components in well-known fields. Conflicts (operator wrote
    /// both `image: repo@sha256:x` AND a `tag:` field) are rejected.
    fn normalize_image_references(&mut self) -> Result<(), ConfigError> {
        for service in &mut self.services {
            if let Some((repo, digest)) = service.image.split_once('@') {
                if !digest.starts_with("sha256:") {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?}.image references `@{}`; only `@sha256:<hex>` digest \
                         references are supported (got {})",
                        service.name, digest, digest
                    )));
                }
                if let Some(existing) = &service.tag
                    && existing != digest
                {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?} pins both an inline digest ({digest}) and a separate \
                         `tag: {existing}` — pick one form",
                        service.name
                    )));
                }
                service.tag = Some(digest.to_string());
                service.image = repo.to_string();
            }
        }
        Ok(())
    }

    /// Add a synthetic `local` host pointing at the local docker
    /// socket — for the operator's laptop daemon, k3d/colima/orbstack
    /// dev environments, or anywhere yoink is run without a real
    /// fleet. Only fires when (a) the config doesn't already declare
    /// a `local` host and (b) one of the platform-default socket
    /// paths exists. No-op on Windows for now (npipe detection
    /// requires a connect attempt rather than a path check).
    ///
    /// Read-only commands (`tui`, `status`, `diff`, `version`) call
    /// this so the dashboard "just works" pointing at your laptop;
    /// destructive commands (`up`, `restart`, `prune`, `rollback`)
    /// don't, since silently deploying to localhost would surprise.
    pub fn push_local_host_if_socket(&mut self) {
        let local = crate::docker_ops::Host::LOCAL_ADDRESS;
        if self.hosts.iter().any(|h| h.address == local) {
            return;
        }
        if !local_socket_exists() {
            return;
        }
        self.hosts.push(HostConfig {
            address: local.to_string(),
            user: local.to_string(),
            ssh_key_secret: None,
        });
    }

    /// Expand `self.include` globs against `self.config_dir`, parse each
    /// match as a [`ConfigFragment`], and append its contributions onto
    /// `self`. Globs that match zero files are allowed (so commenting
    /// out a fragment doesn't break the main config); a glob with an
    /// invalid pattern is a hard error.
    fn merge_includes(&mut self) -> Result<(), ConfigError> {
        if self.include.is_empty() {
            return Ok(());
        }
        let base = self.config_dir.clone().unwrap_or_else(|| ".".into());
        // Collect matched paths first, sort for deterministic order
        // (filesystems return entries in arbitrary order; keeping the
        // service list stable means spec hashes don't churn between
        // identical configs on different boxes).
        let mut matched: Vec<std::path::PathBuf> = Vec::new();
        for pattern in &self.include {
            let abs_pat = if Path::new(pattern).is_absolute() {
                pattern.clone()
            } else {
                base.join(pattern).display().to_string()
            };
            let entries = glob::glob(&abs_pat).map_err(|source| ConfigError::GlobPattern {
                pattern: pattern.clone(),
                source,
            })?;
            for entry in entries {
                let path = entry.map_err(|source| ConfigError::GlobWalk {
                    pattern: pattern.clone(),
                    source,
                })?;
                matched.push(path);
            }
        }
        matched.sort();
        for path in matched {
            let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })?;
            let fragment: ConfigFragment = yaml_serde::from_str(&text)?;
            self.services.extend(fragment.services);
            self.hooks.pre_deploy.extend(fragment.hooks.pre_deploy);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), ConfigError> {
        if self.deploy.networks.is_empty() {
            return Err(ConfigError::Invalid(
                "deploy.networks must declare at least one network".into(),
            ));
        }
        if self.hosts.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one entry under `hosts:` required".into(),
            ));
        }
        for (i, host) in self.hosts.iter().enumerate() {
            if host.address.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hosts[{i}].address must not be empty"
                )));
            }
            if host.user.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hosts[{i}].user must not be empty"
                )));
            }
        }
        let mut host_addresses: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for host in &self.hosts {
            if !host_addresses.insert(host.address.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate hosts.address {:?}",
                    host.address
                )));
            }
        }

        // Top-level network declarations: each entry must be unique
        // and non-empty. Services can only attach to networks
        // declared here.
        let mut declared_networks: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for n in &self.deploy.networks {
            if n.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "deploy.networks entries must not be empty".into(),
                ));
            }
            if !declared_networks.insert(n.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate deploy.networks entry {n:?}"
                )));
            }
        }

        if let Some(SecretsConfig::Age { recipients, .. }) = &self.secrets
            && recipients.is_empty()
        {
            // Without recipients, every `yoink secrets edit/seal/rotate`
            // would fail at the encrypt step with a generic "no
            // recipients" error from the sealed module. Catch it at
            // config-load time so the operator sees the actionable
            // message, not the internals.
            return Err(ConfigError::Invalid(
                "secrets.recipients: must list at least one age public key (age1...) — `yoink secrets key generate` prints one".into(),
            ));
        }

        if self.services.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one entry under `services:` required".into(),
            ));
        }
        let mut seen_names = std::collections::HashSet::new();
        for service in &self.services {
            validate_service_name(&service.name)?;
            if !seen_names.insert(service.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate [[service]] name {:?}",
                    service.name
                )));
            }
            if service.image.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.image must not be empty",
                    service.name
                )));
            }
            // `tag` is optional in the schema; when absent, the
            // operator must pass it via `--tag` at deploy time. We only
            // reject the explicitly-empty case here (`tag: ""`), which
            // is almost always a typo.
            if service.tag.as_deref().is_some_and(|t| t.trim().is_empty()) {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.tag is set but empty; remove the field or supply a value",
                    service.name
                )));
            }
            if let Some(subset) = &service.hosts {
                for h in subset {
                    if !host_addresses.contains(h.as_str()) {
                        return Err(ConfigError::Invalid(format!(
                            "service {:?}.hosts references unknown host {h:?}",
                            service.name
                        )));
                    }
                }
            }
            if service.run.replicas == 0 {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.replicas must be >= 1",
                    service.name
                )));
            }
            if service.run.replicas > 1 && !service.run.publish.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.replicas > 1 is incompatible with publish: \
                     host port bindings are exclusive — only one replica can bind",
                    service.name
                )));
            }
            if let Some(p) = &service.run.healthcheck_path
                && !p.starts_with('/')
            {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.healthcheck_path must start with '/'",
                    service.name
                )));
            }
            if service.run.healthcheck_path.is_some() && service.run.port.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.port required when healthcheck_path is set",
                    service.name
                )));
            }
            // depends_on validation (refs + duplicates + self). Cycle
            // detection lives in `topo_sort_services` since it needs
            // the full graph anyway.
            let mut seen_deps = std::collections::HashSet::new();
            for dep in &service.depends_on {
                if dep == &service.name {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?}.depends_on lists itself",
                        service.name
                    )));
                }
                if !seen_deps.insert(dep.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?}.depends_on contains duplicate {dep:?}",
                        service.name
                    )));
                }
                // Forward references are fine — every name has been
                // collected into `seen_names` already (this loop's
                // earlier branch).
                if !seen_names.contains(dep.as_str())
                    && !self.services.iter().any(|s| s.name == *dep)
                {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?}.depends_on references unknown service {dep:?}",
                        service.name
                    )));
                }
            }

            if let Some(nets) = &service.networks {
                if nets.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "service {:?}.networks is set but empty; remove the field to attach to every \
                         deploy.networks entry",
                        service.name
                    )));
                }
                let mut seen = std::collections::HashSet::new();
                for n in nets {
                    if !declared_networks.contains(n.as_str()) {
                        return Err(ConfigError::Invalid(format!(
                            "service {:?}.networks references undeclared network {n:?} \
                             (declare it under deploy.networks)",
                            service.name
                        )));
                    }
                    if !seen.insert(n.as_str()) {
                        return Err(ConfigError::Invalid(format!(
                            "service {:?}.networks contains duplicate {n:?}",
                            service.name
                        )));
                    }
                }
            }
        }

        // (depends_on cycle detection deferred to topo_sort_services
        //  which has the full graph in front of it.)

        // Hook tag refs must point at a real service.
        for hook in &self.hooks.pre_deploy {
            if hook.image.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hook {:?}.image must not be empty",
                    hook.name
                )));
            }
            if let HookTag::Ref { service } = &hook.tag
                && !seen_names.contains(service.as_str())
            {
                return Err(ConfigError::Invalid(format!(
                    "hook {:?}.tag references unknown service {service:?}",
                    hook.name
                )));
            }
        }

        Ok(())
    }

    /// Reorder `self.services` so every service appears AFTER every
    /// service in its `depends_on`. Stable: among services whose deps
    /// are all already emitted, the one earliest in the original order
    /// wins. Detects cycles and surfaces them with the names involved.
    ///
    /// Called once at the end of `load_from_path` / `parse_str`, so
    /// every downstream consumer (`reconcile`, `prefetch_images`, the
    /// dashboard, etc.) sees `config.services` in deploy order without
    /// having to know the graph exists.
    fn topo_sort_services(&mut self) -> Result<(), ConfigError> {
        let n = self.services.len();
        if n == 0 {
            return Ok(());
        }

        // Original index for stable tie-breaking.
        let original: std::collections::BTreeMap<String, usize> = self
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();

        // Outstanding deps per service (mutated as we emit).
        let mut remaining: Vec<std::collections::BTreeSet<String>> = self
            .services
            .iter()
            .map(|s| s.depends_on.iter().cloned().collect())
            .collect();
        let mut emitted = vec![false; n];
        let mut order: Vec<usize> = Vec::with_capacity(n);

        loop {
            // Pick the earliest-in-original-order service whose deps
            // are all already emitted.
            let next = (0..n).find(|&i| !emitted[i] && remaining[i].is_empty());
            let Some(i) = next else {
                break;
            };
            emitted[i] = true;
            order.push(i);
            let name = self.services[i].name.clone();
            for set in &mut remaining {
                set.remove(&name);
            }
        }

        if order.len() != n {
            // Anything still unemitted is in a cycle. Report the
            // first cycle we can walk so the operator gets a useful
            // pointer instead of "there's a cycle somewhere".
            let stuck: Vec<&str> = (0..n)
                .filter(|&i| !emitted[i])
                .map(|i| self.services[i].name.as_str())
                .collect();
            let cycle = walk_cycle(&self.services, &stuck);
            return Err(ConfigError::Invalid(format!(
                "depends_on cycle detected: {} (services in cycle: {})",
                cycle.join(" → "),
                stuck.join(", "),
            )));
        }

        // Apply the new order.
        let mut sorted: Vec<ServiceConfig> = Vec::with_capacity(n);
        let mut taken = vec![None; n];
        for (slot, src) in self.services.drain(..).enumerate() {
            taken[slot] = Some(src);
        }
        for i in &order {
            sorted.push(taken[*i].take().expect("each index emitted once"));
        }
        self.services = sorted;
        // `original` map is captured for debug-symbol pretty-printing
        // only; binding kept to avoid a "unused" warning loop in case
        // someone later wants to log the before/after.
        let _ = original;
        Ok(())
    }
}

/// Walk one concrete cycle through the `depends_on` graph among a set of
/// services known to be involved in some cycle. Returns names in the
/// order visited, with the start name appearing twice (start … start)
/// so the printed cycle reads naturally.
fn walk_cycle(all: &[ServiceConfig], stuck: &[&str]) -> Vec<String> {
    let stuck_set: std::collections::HashSet<&str> = stuck.iter().copied().collect();
    let by_name: std::collections::HashMap<&str, &ServiceConfig> =
        all.iter().map(|s| (s.name.as_str(), s)).collect();
    let Some(&start) = stuck.first() else {
        return Vec::new();
    };
    let mut path: Vec<String> = vec![start.into()];
    let mut current = start;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(start.into());
    loop {
        let svc = by_name.get(current).copied();
        let next = svc
            .and_then(|s| s.depends_on.iter().find(|d| stuck_set.contains(d.as_str())))
            .map(String::as_str);
        let Some(next) = next else {
            return path;
        };
        path.push(next.into());
        if !seen.insert(next.into()) {
            return path;
        }
        current = next;
    }
}

impl ServiceConfig {
    /// The set of hosts this service should run on, intersected against
    /// the global [[hosts]] list.
    #[must_use]
    pub fn applicable_hosts<'a>(&'a self, all: &'a [HostConfig]) -> Vec<&'a HostConfig> {
        match &self.hosts {
            Some(subset) => all
                .iter()
                .filter(|h| subset.iter().any(|addr| addr == &h.address))
                .collect(),
            None => all.iter().collect(),
        }
    }
}

fn validate_service_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::Invalid(
            "service.name must not be empty".into(),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty above");
    if !first.is_ascii_alphanumeric() {
        return Err(ConfigError::Invalid(format!(
            "service.name {name:?} must start with an alphanumeric character"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(ConfigError::Invalid(format!(
                "service.name {name:?} contains invalid character {c:?} (allowed: A-Z a-z 0-9 . _ -)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn minimal() -> &'static str {
        r#"
hosts:
  - address: host-a
    user: deploy

services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run:
      port: 3000
      healthcheck_path: /health
"#
    }

    /// Round-trip a Config + `ConfigFragment` via `load_from_path`, using a
    /// temp dir so glob expansion against `config_dir` works the same
    /// way it does in production.
    fn write_temp_tree(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("yoink-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, body) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, body).unwrap();
        }
        dir
    }

    #[test]
    fn include_glob_appends_services_from_fragments() {
        let dir = write_temp_tree(&[
            (
                "yoink.yaml",
                r#"
deploy:
  networks: [kamal]
hosts:
  - { address: h1, user: root }
include:
  - services/*.yaml
"#,
            ),
            (
                "services/api.yaml",
                r#"
services:
  - name: api
    image: img/api
    tag: v1
    run: { port: 3000, healthcheck_path: /health }
"#,
            ),
            (
                "services/web.yaml",
                r#"
services:
  - name: web
    image: img/web
    tag: v1
    run: { port: 8080, healthcheck_path: /healthz }
hooks:
  pre_deploy:
    - name: migrate
      image: img/api
      tag: { service: api }
      cmd: ["bin/migrate"]
"#,
            ),
        ]);
        let cfg = Config::load_from_path(&dir.join("yoink.yaml")).unwrap();
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        // sorted by file name ⇒ api before web
        assert_eq!(names, vec!["api", "web"]);
        assert_eq!(cfg.deploy.networks, vec!["kamal".to_string()]);
        assert_eq!(cfg.hooks.pre_deploy.len(), 1);
        assert_eq!(cfg.hooks.pre_deploy[0].name, "migrate");
    }

    #[test]
    fn include_glob_with_zero_matches_is_ok() {
        let dir = write_temp_tree(&[(
            "yoink.yaml",
            r#"
hosts:
  - { address: h1, user: root }
services:
  - name: a
    image: img
    tag: v1
    run: {}
include:
  - "missing/*.yaml"
"#,
        )]);
        let cfg = Config::load_from_path(&dir.join("yoink.yaml")).unwrap();
        assert_eq!(cfg.services.len(), 1);
    }

    #[test]
    fn duplicate_service_across_files_is_rejected() {
        let dir = write_temp_tree(&[
            (
                "yoink.yaml",
                r#"
hosts:
  - { address: h1, user: root }
services:
  - name: dupe
    image: img
    tag: v1
    run: {}
include:
  - "fragments/*.yaml"
"#,
            ),
            (
                "fragments/dupe.yaml",
                r#"
services:
  - name: dupe
    image: img
    tag: v2
    run: {}
"#,
            ),
        ]);
        let err = Config::load_from_path(&dir.join("yoink.yaml")).unwrap_err();
        assert!(format!("{err}").contains("dupe"), "got: {err}");
    }

    #[test]
    fn parses_minimal() {
        let c = Config::parse_str(minimal()).unwrap();
        assert_eq!(c.deploy.networks, vec!["yoink".to_string()]);
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.services.len(), 1);
        assert_eq!(c.services[0].name, "app-a");
        assert_eq!(c.services[0].tag.as_deref(), Some("latest"));
        assert_eq!(c.services[0].run.port, Some(3000));
        assert_eq!(
            c.services[0].run.healthcheck_path.as_deref(),
            Some("/health")
        );
        assert!(c.hooks.pre_deploy.is_empty());
    }

    #[test]
    fn parses_multiple_services() {
        let s = r#"
hosts:
  - { address: host-a, user: root }

services:
  - name: api
    image: registry/bt-api
    tag: a1b2c3d
    run:
      port: 8080
      healthcheck_path: /health
  - name: caddy
    image: caddy
    tag: 2-alpine
    run:
      publish: ["80:80", "443:443"]
      binds: ["/root/certs:/certs:ro"]
      volumes: ["caddy-data:/data"]
      options:
        cap_drop: [ALL]
        cap_add: [NET_BIND_SERVICE]
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(c.services.len(), 2);
        assert_eq!(c.services[0].name, "api");
        assert_eq!(c.services[1].name, "caddy");
        assert_eq!(c.services[1].run.publish, vec!["80:80", "443:443"]);
        assert_eq!(c.services[1].run.binds, vec!["/root/certs:/certs:ro"]);
        assert_eq!(c.services[1].run.volumes, vec!["caddy-data:/data"]);
        assert_eq!(c.services[1].run.options.cap_drop, vec!["ALL"]);
        assert!(c.services[1].run.healthcheck_path.is_none());
    }

    #[test]
    fn parses_pre_deploy_hooks() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 8080, healthcheck_path: /h }

hooks:
  pre_deploy:
    - name: bt-migrate
      image: registry/bt-api
      tag: { service: api }
      entrypoint: [/usr/local/bin/bt-migrate]
      secrets: [DATABASE_MIGRATE_URL]
    - name: auth-migrate
      image: registry/bt-web
      tag: latest
      entrypoint: [node]
      cmd: [/app/dist/server/migrate.js]
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(c.hooks.pre_deploy.len(), 2);
        assert_eq!(c.hooks.pre_deploy[0].name, "bt-migrate");
        assert!(matches!(
            c.hooks.pre_deploy[0].tag,
            HookTag::Ref { ref service } if service == "api"
        ));
        assert!(matches!(
            c.hooks.pre_deploy[1].tag,
            HookTag::Literal(ref t) if t == "latest"
        ));
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let bad = format!("{}\nzomg: true\n", minimal());
        let err = Config::parse_str(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_duplicate_service_names() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 8080, healthcheck_path: /h }
  - name: api
    image: i
    tag: v2
    run: { port: 8081, healthcheck_path: /h }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("duplicate")));
    }

    #[test]
    fn rejects_zero_services() {
        let s = "hosts:\n  - { address: h, user: u }\n";
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("services")));
    }

    #[test]
    fn rejects_zero_hosts() {
        let s = r#"
services:
  - { name: x, image: i, tag: v1, run: { port: 1 } }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("hosts")));
    }

    #[test]
    fn rejects_healthcheck_path_without_port() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: x
    image: i
    tag: v1
    run: { healthcheck_path: /h }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(m) if m.contains("port required")));
    }

    #[test]
    fn rejects_unknown_secret_provider() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: x
    image: i
    tag: v1
    run: { port: 1 }

secrets:
  provider: vault
  project_id: p
  environment: prod
"#;
        // `SecretsConfig` is a tagged enum (`#[serde(tag = "provider")]`);
        // an unknown discriminator like "vault" fails during serde
        // deserialization, so this surfaces as a Parse error (not a
        // post-parse validate error). Either branch confirms bogus
        // providers don't load — the test only cares that they don't.
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Parse(_) | ConfigError::Invalid(_)
        ));
    }

    #[test]
    fn rejects_hook_tag_ref_to_unknown_service() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 1 }

hooks:
  pre_deploy:
    - name: x
      image: i
      tag: { service: nope }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(m) if m.contains("references unknown service")));
    }

    #[test]
    fn applicable_hosts_filters_subset() {
        let s = r#"
hosts:
  - { address: host-a, user: u }
  - { address: host-b, user: u }

services:
  - name: x
    image: i
    tag: v1
    hosts: [host-a]
    run: { port: 1 }
"#;
        let c = Config::parse_str(s).unwrap();
        let applicable = c.services[0].applicable_hosts(&c.hosts);
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].address, "host-a");
    }

    fn cfg_with_services(spec: &[(&str, &[&str])]) -> Config {
        use std::fmt::Write as _;
        let mut s = String::from("hosts:\n  - { address: h, user: u }\nservices:\n");
        for (name, deps) in spec {
            let _ = write!(
                s,
                "  - name: {name}\n    image: i\n    tag: v\n    run: {{ port: 1 }}\n",
            );
            if !deps.is_empty() {
                s.push_str("    depends_on: [");
                s.push_str(&deps.join(", "));
                s.push_str("]\n");
            }
        }
        Config::parse_str(&s).expect("config must parse")
    }

    #[test]
    fn normalize_splits_inline_digest_into_image_and_tag() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: registry.example.com/bt-api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    run: { port: 1 }
"#;
        let cfg = Config::parse_str(s).expect("config must parse");
        let svc = &cfg.services[0];
        assert_eq!(svc.image, "registry.example.com/bt-api");
        assert_eq!(
            svc.tag.as_deref(),
            Some("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn normalize_rejects_inline_digest_when_tag_already_set() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: registry.example.com/bt-api@sha256:abcd
    tag: sha256:ef01
    run: { port: 1 }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("pick one form")));
    }

    #[test]
    fn normalize_rejects_non_sha256_inline_digest() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: registry.example.com/bt-api@latest
    run: { port: 1 }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("only `@sha256:")));
    }

    #[test]
    fn topo_sort_preserves_order_when_no_deps() {
        let cfg = cfg_with_services(&[("a", &[]), ("b", &[]), ("c", &[])]);
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_sort_orders_dependents_after_dependencies() {
        // Declared order: api, redis, otel; api depends on the other two.
        // Sort should place redis + otel BEFORE api.
        let cfg = cfg_with_services(&[("api", &["redis", "otel"]), ("redis", &[]), ("otel", &[])]);
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names[2], "api");
        assert!(names[..2].contains(&"redis"));
        assert!(names[..2].contains(&"otel"));
    }

    #[test]
    fn topo_sort_is_stable_for_independent_services() {
        // pgadmin and redis have no deps; pgadmin appears first in
        // config, so it must stay first.
        let cfg = cfg_with_services(&[("pgadmin", &[]), ("redis", &[]), ("api", &["redis"])]);
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["pgadmin", "redis", "api"]);
    }

    #[test]
    fn topo_sort_rejects_cycles() {
        // a → b → a
        let err = std::panic::catch_unwind(|| cfg_with_services(&[("a", &["b"]), ("b", &["a"])]));
        assert!(err.is_err(), "expected parse failure for cycle");
    }

    #[test]
    fn rejects_unknown_depends_on_reference() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: a
    image: i
    tag: v
    depends_on: [ghost]
    run: { port: 1 }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("unknown service")));
    }

    #[test]
    fn rejects_self_dependency() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: a
    image: i
    tag: v
    depends_on: [a]
    run: { port: 1 }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("itself")));
    }

    #[test]
    fn parses_examples_yoink_yaml() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("examples/yoink.yaml");
        let _ = Config::load_from_path(&path).expect("examples/yoink.yaml must parse");
    }

    #[test]
    fn run_options_default_runs_as_non_root() {
        // The whole hardened-defaults profile in one assertion. If
        // any of these flip silently the security-defaults docs and
        // the actual runtime drift apart.
        let opts = RunOptions::default();
        assert_eq!(opts.user.as_deref(), Some("65534:65534"));
        assert_eq!(opts.cap_drop, vec!["ALL"]);
        assert_eq!(opts.security_opt, vec!["no-new-privileges:true"]);
        assert!(opts.read_only);
        assert!(opts.init);
        assert_eq!(opts.pids_limit, Some(1024));
    }

    #[test]
    fn services_inherit_non_root_default_when_user_unset() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: i
    tag: v1
    run: { port: 8080 }
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(
            c.services[0].run.options.user.as_deref(),
            Some("65534:65534")
        );
    }

    #[test]
    fn services_can_override_user_to_root() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: otel
    image: i
    tag: v1
    run:
      port: 13133
      options:
        user: "0:0"
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(c.services[0].run.options.user.as_deref(), Some("0:0"));
    }

    #[test]
    fn any_service_requires_pull_false_when_all_buildable() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: api
    tag: dev
    build: { context: . }
    run: { port: 8080, healthcheck_path: / }
  - name: web
    image: web
    tag: dev
    build: { context: . }
    run: { port: 3000, healthcheck_path: / }
"#;
        let c = Config::parse_str(s).unwrap();
        assert!(!c.any_service_requires_pull());
    }

    #[test]
    fn any_service_requires_pull_true_with_one_pulled() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: api
    image: api
    tag: dev
    build: { context: . }
    run: { port: 8080, healthcheck_path: / }
  - name: db
    image: postgres
    tag: "16"
    run: { port: 5432, healthcheck_path: / }
"#;
        let c = Config::parse_str(s).unwrap();
        assert!(c.any_service_requires_pull());
    }

    #[test]
    fn requires_configured_registry_true_when_image_is_on_configured_server() {
        let s = r#"
hosts:
  - { address: h, user: u }
registry:
  server: ghcr.io
  username_secret: U
  password_secret: P
secrets:
  provider: command
  command: ["sh", "-c", "echo X=1"]
services:
  - name: api
    image: ghcr.io/me/api
    tag: v1
    run: { port: 8080, healthcheck_path: / }
"#;
        let c = Config::parse_str(s).unwrap();
        assert!(c.requires_configured_registry());
    }

    #[test]
    fn requires_configured_registry_false_when_image_is_on_different_registry() {
        let s = r#"
hosts:
  - { address: h, user: u }
registry:
  server: 4db05qgnlk.registry.depot.dev
  username_secret: U
  password_secret: P
secrets:
  provider: command
  command: ["sh", "-c", "echo X=1"]
services:
  - name: db
    image: postgres
    tag: "16"
    run: { port: 5432, healthcheck_path: / }
  - name: vendor
    image: ghcr.io/u/vendor
    tag: v1
    run: { port: 9000, healthcheck_path: / }
"#;
        let c = Config::parse_str(s).unwrap();
        assert!(!c.requires_configured_registry());
    }

    #[test]
    fn requires_configured_registry_false_when_only_buildable_uses_server() {
        let s = r#"
hosts:
  - { address: h, user: u }
registry:
  server: ghcr.io
  username_secret: U
  password_secret: P
secrets:
  provider: command
  command: ["sh", "-c", "echo X=1"]
services:
  - name: api
    image: ghcr.io/me/api
    tag: v1
    build: { context: . }
    run: { port: 8080, healthcheck_path: / }
"#;
        let c = Config::parse_str(s).unwrap();
        // Buildable services don't drive a pull, so the configured
        // registry's creds are never used.
        assert!(!c.requires_configured_registry());
        // And nothing requires a pull at all.
        assert!(!c.any_service_requires_pull());
    }
}
