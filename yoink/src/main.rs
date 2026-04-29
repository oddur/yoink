// Output discipline (clig.dev: stdout vs stderr).
//
// - **stdout** holds the *primary result* of a command — anything a
//   downstream program might want to consume. `yoink dump`'s JSON,
//   `yoink history`'s rows, `yoink status`'s table, `yoink pull`'s
//   ✓/✗ lines. If you're tempted to `| jq` or `| grep` it, it goes
//   here.
// - **stderr** holds *progress, status, and diagnostic* output —
//   everything else. Deploy events, "rolling back service…", the
//   summary line after `yoink up` (a human report, not a machine
//   record), warnings, errors. Routing these to stderr keeps stdout
//   pipeable.
//
// The `--quiet`/`-q` flag suppresses informational stderr lines (one
// `-q` for most, `-qq` for everything except the bail-out error).
// `--verbose`/`-v` raises `tracing` log verbosity but does not
// affect the stdout/stderr split.
//
// Colour follows `NO_COLOR` (clig.dev) and TTY detection. The `hl`
// log highlighter spawned from the TUI inherits the same decision.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use yoink::config::Config;
use yoink::deploy;
use yoink::docker_ops::{DockerOps, Host, RealDockerOps};
use yoink::git;
use yoink::output;
use yoink::secrets::{self, SecretsBundle};
use yoink::status::StatusReport;
use yoink::tui::{self, Mode};

#[derive(Parser)]
#[command(
    name = "yoink",
    version,
    about = "Small, opinionated container deploy CLI.",
    long_about = "Small, opinionated container deploy CLI.\n\
                  \n\
                  CONFIGURATION:\n\
                  Precedence: CLI flags > environment variables > yoink.yaml > defaults.\n\
                  \n\
                  ENVIRONMENT VARIABLES:\n  \
                    RUST_LOG               raw tracing filter (overrides --verbose)\n  \
                    YOINK_AGE_KEY          age secret key (raw, AGE-SECRET-KEY-1…)\n  \
                    YOINK_AGE_KEY_FILE     path to an age secret key file\n  \
                    EDITOR / VISUAL        editor for `yoink secrets edit`\n  \
                    NO_COLOR               disable coloured output (any value)\n  \
                    PAGER                  pager command for long output (default: less)\n  \
                    XDG_STATE_HOME / HOME  log file directory (TUI mode)\n\
                  \n\
                  Run any subcommand with --help for details."
)]
struct Cli {
    /// Path to the yoink.yaml config file.
    #[arg(short, long, default_value = "yoink.yaml", global = true)]
    config: PathBuf,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress informational stderr output (-q most, -qq all but errors).
    /// Does not affect stdout — primary results stay readable.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    quiet: u8,

    /// When to colour output. `auto` (the default) detects whether the
    /// destination is a terminal; `always` forces colour, `never`
    /// disables it. Honours `NO_COLOR` regardless.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    color: ColorChoice,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Resolve to a concrete on/off decision for a given output stream.
    /// Honours `NO_COLOR` (clig.dev) which always wins over `auto`.
    fn enabled(self, stream_is_tty: bool) -> bool {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => stream_is_tty,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Generate a starter `yoink.yaml` for the current repo, plus an
    /// age identity for sealed secrets. Detects cwd / Dockerfile /
    /// git remote / `~/.ssh/config` and writes a validated config with
    /// zero prompts in the happy path; the identity lands at
    /// `~/.config/yoink/keys/<recipient>.key` (mode 0600) and yoink
    /// finds it automatically next time. Back up the printed key —
    /// it's the only thing that decrypts what you'll seal.
    ///
    /// Pass HOST as a positional arg when ssh config can't infer one.
    /// `--interactive` for stdio prompts. `--no-secrets` to skip the
    /// identity generation (e.g. you'll bring your own key, or use
    /// `provider: command` for secrets).
    Init {
        /// Ssh target (e.g. `deploy@prod-eu-1` or just `prod-eu-1`).
        /// Optional when `~/.ssh/config` has a non-wildcard Host
        /// yoink can use.
        host: Option<String>,
        /// Overwrite an existing `yoink.yaml`.
        #[arg(long)]
        force: bool,
        /// Prompt for every field (defaults match the inferred
        /// values; hit Enter to accept each).
        #[arg(long)]
        interactive: bool,
        /// Override the inferred service name.
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
        /// Override the inferred port (default: Dockerfile EXPOSE
        /// or 8080).
        #[arg(long)]
        port: Option<u16>,
        /// Don't include `port:` / `healthcheck_path:` in the config
        /// (for services with no HTTP surface).
        #[arg(long)]
        no_port: bool,
        /// Override the inferred image reference.
        #[arg(long, value_name = "PATH")]
        image: Option<String>,
        /// Skip generating an age identity. By default `init` writes
        /// a fresh keypair to `~/.config/yoink/keys/<recipient>.key`
        /// and renders the matching `secrets:` block into yoink.yaml,
        /// so the project is ready for `yoink secrets edit` and
        /// `yoink add` immediately. Use this flag when you'll bring
        /// your own key, or when the project will use
        /// `provider: command` for secrets.
        #[arg(long)]
        no_secrets: bool,
    },
    /// Drop a vetted template into your repo — accessory (postgres,
    /// redis, …) or full app (openclaw, …). Fetches from GitHub,
    /// runs a wizard for variable substitution, seals any generated
    /// secrets, extends `yoink.yaml`'s `include:` list. Use a bare
    /// name (`postgres`) for the bundled set, `gh:owner/repo/path`
    /// for arbitrary 3rd-party templates.
    Add {
        /// Template ref. Bare (`postgres`), pinned (`postgres@<sha>`),
        /// or `gh:owner/repo[@ref]/path` for an external source.
        /// Mutually exclusive with `--from-path`. Omit both to open
        /// the interactive picker (or pipe `yoink add` for a
        /// scriptable list dump).
        r#ref: Option<String>,
        /// Use a local directory as the template source instead of
        /// fetching from GitHub. For template authors iterating on a
        /// manifest without push-pull cycles. The path must contain
        /// a `template.yaml`.
        #[arg(long, value_name = "PATH", conflicts_with = "ref")]
        from_path: Option<PathBuf>,
        /// Skip every confirmation prompt (use defaults). Required in
        /// non-interactive contexts (CI).
        #[arg(long)]
        yes: bool,
        /// Run `yoink up` immediately after the fragment is in place.
        /// `kind: app` templates default to yes when interactive.
        #[arg(long)]
        up: bool,
        /// Re-resolve a branch/tag ref to the latest SHA, bypassing the
        /// local cache mapping. Pinned-SHA refs are unaffected.
        #[arg(long)]
        refresh: bool,
        /// Set a manifest variable. Repeatable: `--var name=db --var version=17`.
        #[arg(long = "var", value_name = "KEY=VALUE")]
        var: Vec<String>,
    },
    /// Verify Docker is reachable on each configured host.
    Preflight {
        /// Wait up to DURATION for each host's docker daemon to become
        /// reachable, polling every 3s with backoff. Useful right after
        /// provisioning a fresh host whose cloud-init is still installing
        /// Docker. Without this flag the check fires once and exits on
        /// first failure. Examples: `--wait 90s`, `--wait 2m`.
        #[arg(long, value_name = "DURATION", value_parser = parse_humantime)]
        wait: Option<Duration>,
    },
    /// Reconcile every service in the config to its desired spec.
    /// Drifted containers (image, env, mounts, options) are swapped
    /// via the healthcheck-gated loop.
    Up {
        /// Restrict to one or more services. Repeatable.
        #[arg(long = "service", short = 's', value_name = "NAME")]
        services: Vec<String>,
        /// Override a service's tag. Format: `name=tag`. Repeatable.
        /// Bare `--tag <sha>` applies to every selected service.
        #[arg(long = "tag", value_name = "[NAME=]TAG")]
        tag: Vec<String>,
        /// Allow operating with a dirty git working tree.
        #[arg(long)]
        allow_dirty: bool,
        /// Print the planned actions (per-host: which services would
        /// be created, updated, or left untouched, plus orphans) and
        /// exit. Connects to each host to inspect current state, but
        /// never mutates anything.
        #[arg(long)]
        dry_run: bool,
        /// Output format for `--dry-run`. `text` is the default
        /// human-readable form; `markdown` is suitable for posting as
        /// a sticky PR comment from CI; `json` is for machine
        /// consumers (e.g. assert with `jq` that no service would be
        /// updated). Ignored when `--dry-run` isn't set.
        #[arg(long, value_enum, default_value_t = DryRunFormat::Text)]
        format: DryRunFormat,
        /// Force local-only mode: ship every selected service's image
        /// from the operator's docker daemon to each host, skipping
        /// all registry pulls. Usually unnecessary — services with a
        /// `build:` block ship from local automatically; this flag
        /// widens that to non-build services too (offline / airgapped
        /// deploys, or shipping a locally-modified public image).
        /// Default transport is `unregistry` — see `--transport`.
        #[arg(long)]
        no_registry: bool,
        /// How `--no-registry` ships images to each host.
        /// `unregistry` (default via `auto`) spins up an ephemeral
        /// `ghcr.io/psviderski/unregistry` sidecar on the host, opens
        /// an SSH-tunnelled local port, and pushes layers over the
        /// OCI registry protocol — only the layers the host doesn't
        /// already have cross the wire (massive win on redeploys).
        /// Push runs from the yoink process directly, so it works
        /// even on macOS Docker Desktop where the daemon lives in a
        /// VM. `tarball` opts out: streams the entire `docker save`
        /// tarball over ssh — slower, no dedup, but zero dependencies
        /// on the host beyond docker. `auto` tries unregistry first
        /// and falls back to tarball with a warning if anything in
        /// the unregistry setup fails (e.g. ghcr unreachable, ssh
        /// forward refused). Ignored without `--no-registry`.
        #[arg(long, value_enum, default_value_t = TransportMode::Auto)]
        transport: TransportMode,
        /// Run `docker build` for any selected service with a
        /// `build:` block before deploying. Eliminates the separate
        /// `yoink build && yoink up` two-step for the standalone
        /// workflow — `yoink up --build --no-registry --service my-tool`
        /// is the indie one-shot. With a registry-prefixed image, you
        /// still need to push (`yoink build --push`) — `--build` here
        /// builds without pushing, so this combination is most useful
        /// alongside `--no-registry`.
        #[arg(long)]
        build: bool,
        /// Force a fresh xcaddy build of the proxy image, skipping the
        /// per-host `image_present` short-circuit. Useful when an
        /// unpinned plugin's underlying module changed upstream — the
        /// hash hasn't moved (the config string is identical) but the
        /// resolved Go module would. Without this flag, an `up`
        /// reuses the cached `yoink-caddy:<hash>` image. With it,
        /// every proxy host re-runs `xcaddy build` and re-tags the
        /// image. No-op when `proxy.xcaddy:` isn't set.
        #[arg(long)]
        rebuild_proxy: bool,
        /// Force a redeploy even when the running container already
        /// matches the desired spec. Skips the at-spec early-return,
        /// so every selected service goes through the full
        /// rolling-swap-with-healthcheck loop and (for routed
        /// services) the Caddy admin push. Useful as a recovery
        /// gesture when proxy-side state has drifted from container
        /// reality (e.g. a stale upstream pool that never got cleaned
        /// up). Combine with `--service <name>` to limit blast radius.
        #[arg(long)]
        force: bool,
        /// Use `git rev-parse --short HEAD` as the tag for every
        /// selected service (or every service when `--service` is
        /// omitted). Equivalent to `--service x --tag $(git rev-parse
        /// --short HEAD)` per service. Conflicts with bare `--tag`.
        #[arg(long, conflicts_with = "tag")]
        here: bool,
        /// Friendlier alias for `--dry-run`. Mirrors `terraform plan`:
        /// print what would change and exit without mutating. With
        /// the default `--format text`, output reads as a per-service
        /// summary; `--format markdown` is suitable for PR comments.
        #[arg(long, conflicts_with = "watch")]
        plan: bool,
        /// Re-reconcile whenever the config changes on disk. Polls
        /// every 2s — same cadence as the TUI's reload tick. Pairs
        /// with `--build --no-registry --service <name>` for the
        /// edit-save-deploy inner loop. Ctrl-C exits.
        #[arg(long)]
        watch: bool,
    },
    /// Build one or more services' images via `docker build` against
    /// the operator's local docker daemon. Tags the result as
    /// `<image>:<tag>`. Pair with `yoink up --no-registry` to deploy
    /// the freshly-built image without any registry. Requires the
    /// service to declare a `build:` block.
    Build {
        /// Restrict to one or more services (those with a `build:`
        /// block). Empty = build every service that has one.
        #[arg(long = "service", short = 's', value_name = "NAME")]
        services: Vec<String>,
        /// Override the resolved tag (same shape as `up --tag`).
        #[arg(long = "tag", value_name = "[NAME=]TAG")]
        tag: Vec<String>,
        /// Allow operating with a dirty git working tree.
        #[arg(long)]
        allow_dirty: bool,
        /// Forward `--no-cache` to `docker build`.
        #[arg(long)]
        no_cache: bool,
        /// Run `docker push <image>:<tag>` after a successful build.
        /// Use this when `image:` points at a real remote registry
        /// (`ghcr.io/you/api`, `4db05qgnlk.registry.depot.dev/api`,
        /// etc.) and you want the kamal-style "build locally, deploy
        /// from registry" loop in a single command. Operator must
        /// already be `docker login`'d to the target registry.
        #[arg(long)]
        push: bool,
    },
    /// Show what's running where (across all services).
    Status {
        /// Emit a structured JSON object instead of the default
        /// human-readable table. Use this from CI to assert post-deploy
        /// state programmatically:
        ///   `task yoink:status -- --json | jq '.hosts[] | .containers[] | select(.state != "running")'`
        #[arg(long)]
        json: bool,
    },
    /// Roll a service back to the most recent previously-deployed tag.
    /// Reads `yoink.version` off the host's exited containers labeled
    /// with this service, picks the newest, and dispatches through the
    /// usual `up` flow (rolling swap + healthcheck + migrations).
    /// Pass `--tag <value>` to skip the discovery step and pin to a
    /// specific tag.
    Rollback {
        /// Service name as declared in the config.
        service: String,
        /// Skip discovery; deploy this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Remove yoink-managed containers that the current config no
    /// longer describes (renamed/removed services) plus stale exited
    /// containers from previous reconciles.
    Prune {
        /// Print what would be removed without actually removing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a one-shot command inside a running container for a service.
    /// Output is captured + printed; exit code mirrors the command's.
    /// For an interactive shell with PTY (bash, etc.) use `yoink shell`.
    Exec {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts; otherwise yoink errors with the candidate list.
        #[arg(long)]
        host: Option<String>,
        /// Command to run, after `--`. e.g. `yoink exec api -- ls -la /app`.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Stream or tail logs from a service's container.
    Logs {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
        /// Follow new log lines until Ctrl-C.
        #[arg(long, short)]
        follow: bool,
        /// Lines of history to show before following (default 50).
        #[arg(long, default_value_t = 50)]
        tail: u32,
    },
    /// Print the currently-running tag(s) for a service across hosts.
    /// One line per replica.
    Version { service: String },
    /// Drop into an interactive PTY shell inside a service's container
    /// (k9s-style, terminal-side). Picks `bash` when present, falls
    /// back to `sh`. With multiple replicas and no `--host`, picks the
    /// first healthy replica. Exit with `exit` or Ctrl-D.
    ///
    /// Aliased as `ssh` for muscle memory.
    #[command(alias = "ssh")]
    Shell {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
    },
    /// Forward a container port to the laptop over SSH.
    ///
    /// Auto-mode: when the service has a matching `publish:` entry,
    /// yoink reuses the host's existing `docker-proxy` binding and
    /// just bridges with `ssh -L` (~50 ms). When it doesn't (api /
    /// web behind Caddy), yoink spawns an ephemeral `alpine/socat`
    /// sidecar on the target's docker network and tunnels through
    /// that (~2 s warm-start). Either way the operator gets a
    /// `localhost:N` URL.
    ///
    /// Usage shapes:
    ///   yoink pf pgadmin               # service has 1 publish; LOCAL = OS-assigned
    ///   yoink pf pgadmin 5050          # localhost:5050 → pgadmin:5050 (or its mapped port)
    ///   yoink pf pgadmin 5050:80       # localhost:5050 → pgadmin:80 (explicit container port)
    ///   yoink pf api 8080 -o           # also opens a browser when the tunnel is ready
    Pf {
        /// Service name as declared in the config.
        service: String,
        /// Either `LOCAL:CONTAINER` or just `CONTAINER` (LOCAL defaults
        /// to the same number as CONTAINER, or use `0:CONTAINER` /
        /// `--local 0` to ask the OS for a free port). Optional when
        /// the service has exactly one `publish:` entry (that entry's
        /// container port is used) or no `publish:` block at all but
        /// declares `run.port:` (the healthcheck port is used).
        #[arg(value_name = "[LOCAL:]CONTAINER_PORT")]
        port: Option<String>,
        /// Pin to a specific host when the service runs on multiple.
        #[arg(long)]
        host: Option<String>,
        /// Replica index (0-based) for services with `replicas: > 1`.
        /// Defaults to 0.
        #[arg(short = 'r', long, default_value_t = 0)]
        replica: usize,
        /// Open a browser at the forwarded URL once the tunnel is up.
        /// Same heuristic as the printed link: HTTP for 80/3000/5050/…,
        /// HTTPS for 443. For non-web ports (databases, gRPC) `--scheme`
        /// can override.
        #[arg(short = 'o', long)]
        open: bool,
        /// Override the URL scheme used by the printed link / `--open`.
        /// `auto` runs the port-based heuristic (default); `http` /
        /// `https` force-wrap; `tcp` / `none` print bare `localhost:N`
        /// and suppress browser-open.
        #[arg(long, value_enum, default_value_t = PfScheme::Auto)]
        scheme: PfScheme,
        /// Print the assigned local port + remote endpoint as a single
        /// JSON line on stdout, then keep tunneling until SIGINT.
        /// Useful for scripts that want to read the chosen port.
        #[arg(long)]
        json: bool,
        /// How to reach the container. `auto` (default) tries the
        /// `publish:` block first and falls back to spawning a
        /// socat sidecar if the service doesn't publish the
        /// requested port — that's the "secure-by-default" path
        /// for services like api/web that only expose to Caddy.
        /// `published` errors instead of spawning. `sidecar`
        /// always spawns even when a publish exists.
        #[arg(long, value_enum, default_value_t = PfMode::Auto)]
        mode: PfMode,
    },
    /// Like `shell` but spawns an `alpine` debug sidecar in the
    /// target's pid+net namespaces — for distroless / shell-less
    /// images. The sidecar is `--rm` and is force-removed on exit.
    Debug {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
        /// Image to use for the sidecar. Defaults to alpine.
        #[arg(long, default_value = "alpine")]
        image: String,
    },
    /// Launch the interactive ratatui dashboard.
    Tui {
        /// Initial mode to open.
        #[arg(long, value_enum, default_value_t = Mode::Dashboard)]
        mode: Mode,
        /// Enable mouse capture — scroll wheel selects rows / scrolls
        /// log views. Off by default because mouse capture takes the
        /// mouse away from the host terminal's native text-selection.
        #[arg(long)]
        mouse: bool,
    },
    /// Bounce a service's container without re-deploying. Stops the
    /// container with the configured drain, then starts it again.
    Restart {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas.
        #[arg(long)]
        host: Option<String>,
    },
    /// SIGKILL a service's container (no graceful drain). Container
    /// stays around for inspect/logs; use `up` or `restart` to bring
    /// it back. Use this when the configured drain isn't fast enough.
    Kill {
        service: String,
        #[arg(long)]
        host: Option<String>,
        /// Skip the "are you sure?" confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Pre-warm a service's image on every host (or a specific one)
    /// without deploying. Useful right before a low-window deploy
    /// where the pull is the longest leg.
    Pull {
        service: String,
        /// Override the tag — same format as `up --tag`.
        #[arg(long)]
        tag: Option<String>,
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Show the deploy history for a service — every yoink-managed
    /// container (running + exited) labeled with the service, sorted
    /// newest-first by deploy time.
    History {
        service: String,
        /// Maximum entries to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output format. `text` is the default human-readable table;
        /// `json` is a stable schema for piping to `jq`.
        #[arg(long, value_enum, default_value_t = TableFormat::Text)]
        format: TableFormat,
    },
    /// `htop`-style snapshot of every running yoink-managed container
    /// across all hosts, sorted by CPU% descending. Single shot —
    /// wrap in \`watch -n 2 yoink top\` for a live display.
    Top {
        /// Maximum rows to print.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Output format. `text` is the default human-readable table;
        /// `json` is a stable schema for piping to `jq`.
        #[arg(long, value_enum, default_value_t = TableFormat::Text)]
        format: TableFormat,
    },
    /// List every docker network across all configured hosts.
    Networks {
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// List every docker volume across all configured hosts.
    Volumes {
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Dense JSON dump of everything yoink can observe — config,
    /// per-host docker info, every yoink-managed container with
    /// inspect data, stats, log tail, and drift status. Designed to
    /// be piped into an LLM/agent for diagnosis: `yoink dump | pbcopy`.
    /// Env values containing secret-ish substrings are redacted.
    Dump {
        /// Lines of recent logs to include per container.
        #[arg(long, default_value_t = 50)]
        log_tail: u32,
    },
    /// Lint the config and (optionally) ping each host's docker daemon.
    /// Use this in CI before merging a yoink.yaml change.
    Validate {
        /// Also test the ssh+docker connection to every configured host.
        #[arg(long)]
        check_hosts: bool,
    },
    /// Diagnose common deploy-blockers before running `yoink up`.
    /// Checks every host is reachable, docker arches align (laptop vs
    /// host for any service with a `build:` block), age identity is
    /// loadable, DNS resolves for `domain:`-tagged services, and
    /// flags configurations that would fail mid-deploy.
    Doctor {
        /// Output as JSON instead of the default human-readable list.
        /// Each entry: `{severity, category, title, detail, fix}`.
        #[arg(long)]
        json: bool,
    },
    /// Render the Caddy admin-API JSON yoink would push for the
    /// current config. Read-only; useful for inspecting the proxy
    /// config or piping into `caddy adapt` / a debug Caddy's `/load`
    /// for schema-validation. Container upstream lookups are skipped
    /// (uses service-name fallbacks) so this works without a host
    /// connection.
    ProxyRender,
    /// Print the synthesized xcaddy Dockerfile for the current
    /// `proxy.xcaddy:` config. No docker calls; useful for code review,
    /// testing the build locally with `docker build -`, or pinning a
    /// specific Dockerfile in CI.
    ProxyDockerfile,
    /// Inspect or release the per-host deploy lock.
    ///
    /// Each `yoink up` acquires a sentinel container as a deploy lock so
    /// concurrent deploys can't interleave their reconciles. On a clean
    /// exit the sentinel goes away. On a crash (laptop closed mid-deploy,
    /// SIGKILL, network partition), the sentinel can outlive the deploy
    /// — `yoink lock status` shows where, and `yoink lock release` clears
    /// it (with `--yes` to confirm, since releasing while another
    /// operator is still deploying corrupts that deploy).
    Lock {
        #[command(subcommand)]
        action: LockAction,
    },
    /// Show what would change between the running container and the
    /// target spec for `service` (image SHA, env vars, ports, mounts).
    Diff {
        /// Service name as declared in the config.
        service: String,
        /// Override the target tag the same way `up --tag` does.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Generate shell completions. e.g.
    ///   `yoink completions zsh > ~/.config/zsh/completions/_yoink`
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Internal: print field values for shell completion. Hidden
    /// because it's plumbing for the bash/zsh snippet documented in
    /// the CLI reference; operators don't invoke it directly.
    #[command(name = "__complete", hide = true)]
    Complete {
        #[arg(value_enum)]
        what: CompleteKind,
    },
    /// Manage `age`-sealed secrets — the batteries-included default.
    ///
    /// `key generate` writes a fresh identity (default: `~/.config/yoink/keys/<recipient>.key`,
    /// mode 0600) and prints the public recipient to paste into yoink.yaml.
    /// `key public` prints the recipient yoink would use right now.
    /// `edit` opens `secrets.age` in your `$EDITOR` as plaintext dotenv,
    /// re-seals on save. `show` prints the masked or revealed contents.
    /// `seal` is the non-interactive form of `edit` (read dotenv from
    /// `--in` / stdin, write to `--out` / `secrets.age`). `rotate` swaps
    /// in a new identity, re-sealing every value against both the old
    /// and new recipients so CI can pick up the change without a flag-day.
    ///
    /// For non-age secrets (provider-managed via Doppler / 1Password /
    /// Vault / etc.) configure `secrets.provider: command` in yoink.yaml
    /// — the `secrets` subcommand only manages age-sealed files.
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },
}

/// CLI value enum mirror of `yoink::pf::SchemeOverride`. Lives in
/// main.rs so it can carry clap's `ValueEnum` derive without
/// pulling clap into the lib crate; converted via `From` on the
/// way into `cmd_pf`.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum PfScheme {
    #[default]
    Auto,
    Http,
    Https,
    Tcp,
    None,
}

impl From<PfScheme> for yoink::pf::SchemeOverride {
    fn from(s: PfScheme) -> Self {
        match s {
            PfScheme::Auto => Self::Auto,
            PfScheme::Http => Self::Http,
            PfScheme::Https => Self::Https,
            PfScheme::Tcp => Self::Tcp,
            PfScheme::None => Self::None,
        }
    }
}

/// CLI value enum for `yoink pf --mode`. `auto` is the default and
/// covers both the published-port fast path and the sidecar
/// fallback. `published` errors when the service doesn't publish;
/// `sidecar` always spawns a socat sidecar even for services that
/// could use the fast path (rare; useful for "I want to bypass
/// docker-proxy and hit the container's :8080 directly").
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum PfMode {
    #[default]
    Auto,
    Published,
    Sidecar,
}

impl From<PfMode> for yoink::pf::Mode {
    fn from(m: PfMode) -> Self {
        match m {
            PfMode::Auto => Self::Auto,
            PfMode::Published => Self::Published,
            PfMode::Sidecar => Self::Sidecar,
        }
    }
}

/// What `yoink __complete` lists. Drives the dynamic-completion
/// snippets in the CLI reference docs — extend as new shell-completion
/// needs surface.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CompleteKind {
    Services,
    Hosts,
    /// Yoink config files reachable from the current working
    /// directory. Walked depth-first, ranked by recency. Powers
    /// `yoink -c <TAB>`.
    Configs,
}

/// Subcommands for `yoink secrets`.
#[derive(clap::Subcommand)]
enum SecretsAction {
    /// Manage the age identity used to (un)seal `secrets.age`. The
    /// identity is just an X25519 keypair; the public half lands in
    /// `secrets.recipients:` of `yoink.yaml`, and the private half
    /// gets routed through whatever secret manager you already use
    /// (GitHub Actions secret, 1Password, AWS Secrets Manager, …).
    /// See `docs/recipes/secrets-*` for per-tool wiring.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Decrypt the sealed file into `$EDITOR`, then re-seal on save.
    /// Creates the file if it doesn't exist yet.
    Edit,
    /// Decrypt and print the sealed file. Values are masked unless
    /// `--reveal` is passed; keys are always shown.
    Show {
        /// Print the actual secret values instead of masks.
        #[arg(long)]
        reveal: bool,
    },
    /// One-shot: read a plaintext dotenv from `--in` (or stdin) OR
    /// individual `--as KEY=...` pairs, merge into a single bundle,
    /// seal against the recipients in `yoink.yaml`, write to
    /// `secrets.age` (or `--out`).
    Seal {
        /// Plaintext dotenv input. `-` (or unset) reads from stdin.
        /// Mutually exclusive with `--as`.
        #[arg(long, value_name = "PATH", conflicts_with = "as_pairs")]
        r#in: Option<PathBuf>,
        /// Set a single key directly: `--as KEY=value` for a literal,
        /// or `--as KEY=@PATH` to read the value from a file (useful
        /// for multi-line PEMs / SSH keys without the dotenv-quoting
        /// dance). Repeatable. Mutually exclusive with `--in`.
        #[arg(long = "as", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
        as_pairs: Vec<String>,
        /// Output path. Defaults to the configured `secrets.file:`
        /// (or `secrets.age` next to `yoink.yaml`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Generate a new identity and re-seal `secrets.age` against
    /// both the existing recipients AND the new public key. Prints
    /// the new secret for pasting into a GitHub Actions secret. After
    /// CI is updated to the new key, edit `yoink.yaml` to remove the
    /// old recipient and run `yoink secrets edit` (just save without
    /// changes) to re-seal under the new recipients only.
    Rotate,
    /// Manage SSH keypairs used by yoink itself (e.g. host
    /// `ssh_key_secret:` references). Subcommands operate on
    /// already-sealed bundles, so the private half never touches disk
    /// in plaintext.
    SshKey {
        #[command(subcommand)]
        action: SshKeyAction,
    },
}

#[derive(clap::Subcommand)]
enum SshKeyAction {
    /// Generate a fresh ed25519 SSH keypair, seal the private half
    /// into the configured `secrets.age` under `--seal-as <NAME>`, and
    /// print the OpenSSH-format public key to stdout (one line, ready
    /// to pipe into `hcloud ssh-key create --public-key-from-file -`,
    /// `gh ssh-key add`, your provider's SSH-key form, etc.).
    ///
    /// The private half is generated in memory and committed straight
    /// into the sealed bundle — no plaintext PEM ever lands in /tmp,
    /// no `shred` cleanup needed. Existing keys in the bundle are
    /// preserved (same merge logic as `yoink secrets edit`).
    Generate {
        /// Name to seal the private key under. Becomes the secret key
        /// referenced by `hosts[].ssh_key_secret:` (or any other yoink
        /// surface that resolves a sealed value by name).
        #[arg(long = "seal-as", value_name = "NAME")]
        seal_as: String,
        /// Optional comment baked into the OpenSSH key header. Mirrors
        /// `ssh-keygen -C`. Default: empty.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
    },
    /// Print the OpenSSH-format public key derived from a sealed SSH
    /// private key, one line on stdout. Useful for piping into provider
    /// SSH-key-upload commands without re-extracting the public half:
    ///
    ///   hcloud ssh-key create --name yoink-scratch \
    ///     --public-key-from-file <(yoink secrets ssh-key public --name DEPLOY_SSH_KEY)
    Public {
        /// Name of the sealed SSH private key to derive the public from.
        #[arg(long, value_name = "NAME")]
        name: String,
    },
}

#[derive(clap::Subcommand)]
enum KeyAction {
    /// Generate a fresh age identity. By default the secret key is
    /// saved to `~/.config/yoink/keys/<public-recipient>.key` (mode
    /// 0o600) and yoink will discover it automatically next time —
    /// no env var, no per-project gitignore. The public recipient is
    /// printed for adding to `secrets.recipients:` in `yoink.yaml`.
    ///
    /// Filename = public recipient means multiple projects with
    /// distinct identities coexist in one dir without collisions.
    ///
    /// Pass `--out PATH` to write to a specific file (e.g. for CI or
    /// to keep the key alongside a project), or `--print` to send the
    /// secret to stdout for piping/pasting yourself.
    Generate {
        /// Write the secret key to PATH (mode 0o600) instead of the
        /// default keys dir. Refuses to overwrite an existing file
        /// unless `--force`.
        #[arg(long, conflicts_with = "print")]
        out: Option<PathBuf>,
        /// Overwrite an existing identity at the destination.
        #[arg(long)]
        force: bool,
        /// Print the secret to stdout instead of writing it to disk.
        /// Use when piping into a CI secret (`yoink secrets key
        /// generate --print | gh secret set YOINK_AGE_KEY`) or a
        /// password manager.
        #[arg(long)]
        print: bool,
    },
    /// Print the public recipient (`age1…`) derived from the
    /// currently-resolved identity. Useful for "is the key in my
    /// shell the same one yoink.yaml expects?" sanity checks.
    /// Identity resolution: `YOINK_AGE_KEY` (raw) →
    /// `YOINK_AGE_KEY_FILE` (path) → `~/.config/yoink/age.key`.
    Public,
}

/// Subcommands for `yoink lock`.
#[derive(clap::Subcommand)]
enum LockAction {
    /// Print holder + age of the deploy lock on each configured host.
    Status,
    /// Force-remove the deploy lock sentinel on each host. Use with
    /// care — only safe when no operator is actually deploying.
    Release {
        /// Restrict to one host instead of all.
        #[arg(long)]
        host: Option<String>,
        /// Skip the "are you sure?" confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// Quiet level: 0 = normal, 1 = `-q` (status lines suppressed),
/// 2+ = `-qq` (everything but the bail-out error suppressed).
/// Set once at startup and read via [`quiet_level`] from anywhere.
static QUIET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn quiet_level() -> u8 {
    QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

/// Use in place of `eprintln!` for *informational* progress lines.
/// Suppressed at `-q` and above. Errors and warnings should remain
/// `eprintln!` so they always reach the user.
macro_rules! info_eprintln {
    ($($arg:tt)*) => {{
        if $crate::quiet_level() == 0 {
            eprintln!($($arg)*);
        }
    }};
}

/// Page `content` through `$PAGER` when stdout is a TTY (clig.dev:
/// "Use pagers for lengthy output"). Falls back to `less -FIRX` when
/// `$PAGER` is unset and to a plain print when `less` is unavailable
/// or stdout is piped. The `-FIRX` flags make `less` quit if the
/// content fits on one screen and avoid clearing it on exit.
fn page_output(content: &str) {
    use std::process::{Command, Stdio};

    if !io::stdout().is_terminal() {
        print!("{content}");
        return;
    }
    let pager_cmd = std::env::var("PAGER").unwrap_or_else(|_| "less -FIRX".to_string());
    let mut parts = pager_cmd.split_whitespace();
    let Some(bin) = parts.next() else {
        print!("{content}");
        return;
    };
    let args: Vec<&str> = parts.collect();
    let Ok(mut child) = Command::new(bin).args(&args).stdin(Stdio::piped()).spawn() else {
        // Pager binary not on PATH — print directly rather than fail.
        print!("{content}");
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
    }
    let _ = child.wait();
}

/// TTY-gated interactive confirmation for destructive operations
/// (clig.dev: severe changes require non-trivial confirmation).
/// Thin wrapper around [`yoink::prompt::confirm`] in `Destructive`
/// mode — kept for the `bail!`-on-reject ergonomic so callers can
/// `confirm_destructive(...)?` without a separate match.
fn confirm_destructive(prompt: &str) -> Result<()> {
    use yoink::prompt::{ConfirmKind, confirm};
    if !confirm(prompt, ConfirmKind::Destructive, false)? {
        anyhow::bail!("aborted");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_tui = matches!(cli.command, Command::Tui { .. });
    QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    // `--color` is parsed and exposed for future colored output; today
    // only `NO_COLOR` is read, by both `tracing` (env-detected) and the
    // TUI's `hl` spawn. Calling `enabled()` here keeps the value alive
    // for clap's --help validation but otherwise is a no-op until a
    // colored output path actually consults it.
    let _ = cli.color.enabled(io::stderr().is_terminal());
    init_logging(cli.verbose, is_tui);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("yoink: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8, is_tui: bool) {
    let default_level = match verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if is_tui {
        // The TUI takes over the alt-screen; tracing on stderr would
        // scribble straight onto the rendered widgets. Send logs to
        // a file under XDG state dir so the operator can `tail -f`
        // them in another terminal during a TUI session.
        let path = log_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        match writer {
            Some(file) => {
                let _ = builder.with_writer(std::sync::Mutex::new(file)).try_init();
            }
            None => {
                // Fall back to a no-op writer — anything's better
                // than letting tracing scribble over the TUI.
                let _ = builder.with_writer(io::sink).try_init();
            }
        }
        eprintln!("yoink tui: logs → {}", path.display());
    } else {
        let _ = builder.with_writer(io::stderr).try_init();
    }
}

/// Per-OS state directory for the TUI log file. Falls back to
/// `/tmp/yoink-tui.log` if neither `XDG_STATE_HOME` nor `HOME` are
/// set.
fn log_file_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("yoink").join("tui.log");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/state/yoink")
            .join("tui.log");
    }
    PathBuf::from("/tmp/yoink-tui.log")
}

#[allow(clippy::too_many_lines)] // one big match dispatch; splitting buys nothing
async fn run(cli: Cli) -> Result<()> {
    if let Some(result) = run_bootstrap(&cli.command) {
        return result;
    }

    // `yoink add` may be the very command that fixes a dangling
    // reference (e.g. `depends_on: [postgres]` written before
    // `yoink add postgres` ran), so it loads through the relaxed
    // path that skips cross-service validation. Every other command
    // assumes a deployable config.
    let config = if matches!(cli.command, Command::Add { .. }) {
        Config::load_from_path_relaxed(&cli.config)
    } else {
        Config::load_from_path(&cli.config)
    }
    .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Add {
            r#ref,
            from_path,
            yes,
            up,
            refresh,
            var,
        } => {
            let outcome = yoink::add::cmd_add(
                &config,
                &cli.config,
                yoink::add::AddOpts {
                    r#ref,
                    from_path,
                    yes,
                    up,
                    refresh,
                    vars: var,
                },
            )
            .await?;
            if outcome.deploy_requested {
                // include: edits + new fragment files mean the in-memory
                // config we loaded above is now stale. Reload before
                // running up so the freshly-added service is included.
                let reloaded = Config::load_from_path(&cli.config)
                    .with_context(|| format!("reloading {}", cli.config.display()))?;
                let no_services: Vec<String> = Vec::new();
                let no_tags: Vec<String> = Vec::new();
                cmd_up(
                    &reloaded,
                    UpOptions {
                        services: &no_services,
                        tag_args: &no_tags,
                        allow_dirty: false,
                        dry_run: false,
                        format: DryRunFormat::Text,
                        no_registry: false,
                        transport: TransportMode::Auto.into(),
                        build: false,
                        rebuild_proxy: false,
                        force: false,
                        here: false,
                        plan: false,
                        watch: false,
                        config_path: &cli.config,
                    },
                )
                .await
            } else {
                Ok(())
            }
        }
        Command::Preflight { wait } => cmd_preflight(&config, wait).await,
        Command::Up {
            services,
            tag,
            allow_dirty,
            dry_run,
            format,
            no_registry,
            transport,
            build,
            rebuild_proxy,
            force,
            here,
            plan,
            watch,
        } => {
            cmd_up(
                &config,
                UpOptions {
                    services: &services,
                    tag_args: &tag,
                    allow_dirty,
                    dry_run,
                    format,
                    no_registry,
                    transport: transport.into(),
                    build,
                    rebuild_proxy,
                    force,
                    here,
                    plan,
                    watch,
                    config_path: &cli.config,
                },
            )
            .await
        }
        Command::Build {
            services,
            tag,
            allow_dirty,
            no_cache,
            push,
        } => {
            cmd_build(
                &config,
                BuildOptions {
                    services: &services,
                    tag_args: &tag,
                    allow_dirty,
                    no_cache,
                    push,
                },
            )
            .await
        }
        Command::Status { json } => cmd_status(&config, json).await,
        Command::Rollback { service, tag } => cmd_rollback(&config, service, tag).await,
        Command::Prune { dry_run } => cmd_prune(&config, dry_run).await,
        Command::Exec { service, host, cmd } => {
            cmd_exec(&config, &service, host.as_deref(), cmd).await
        }
        Command::Logs {
            service,
            host,
            follow,
            tail,
        } => cmd_logs(&config, &service, host.as_deref(), follow, tail).await,
        Command::Version { service } => cmd_version(&config, &service).await,
        Command::Shell { service, host } => {
            cmd_pty(&config, &service, host.as_deref(), PtyMode::Exec).await
        }
        Command::Pf {
            service,
            port,
            host,
            replica,
            open,
            scheme,
            json,
            mode,
        } => {
            cmd_pf(
                &config,
                &service,
                port.as_deref(),
                host.as_deref(),
                replica,
                open,
                scheme.into(),
                json,
                mode.into(),
            )
            .await
        }
        Command::Debug {
            service,
            host,
            image,
        } => cmd_pty(&config, &service, host.as_deref(), PtyMode::Debug { image }).await,
        Command::Restart { service, host } => cmd_restart(&config, &service, host.as_deref()).await,
        Command::Kill { service, host, yes } => {
            cmd_kill(&config, &service, host.as_deref(), yes).await
        }
        Command::Pull { service, tag, host } => {
            cmd_pull(&config, &service, tag.as_deref(), host.as_deref()).await
        }
        Command::History {
            service,
            limit,
            format,
        } => cmd_history(&config, &service, limit, format).await,
        Command::Top { limit, format } => cmd_top(&config, limit, format).await,
        Command::Networks { host } => cmd_networks(&config, host.as_deref()).await,
        Command::Volumes { host } => cmd_volumes(&config, host.as_deref()).await,
        Command::Dump { log_tail } => cmd_dump(&config, log_tail).await,
        Command::Validate { check_hosts } => cmd_validate(&config, check_hosts).await,
        Command::Doctor { json } => cmd_doctor(&config, json).await,
        Command::ProxyRender => cmd_proxy_render(&config).await,
        Command::ProxyDockerfile => cmd_proxy_dockerfile(&config),
        Command::Lock { action } => cmd_lock(&config, action).await,
        Command::Diff { service, tag } => cmd_diff(&config, &service, tag.as_deref()).await,
        Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
        Command::Complete { what } => {
            cmd_complete(&config, what);
            Ok(())
        }
        Command::Secrets { action } => cmd_secrets(&config, action),
        Command::Tui { mode, mouse } => cmd_tui(&config, cli.config.clone(), mode, mouse).await,
        Command::Init { .. } => unreachable!("init handled by run_bootstrap"),
    }
}

async fn cmd_preflight(config: &Config, wait: Option<Duration>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let mut had_error = false;
    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        match probe_host_with_wait(&ops, &host, wait).await {
            Ok(v) => eprintln!(
                "✓ {}: docker {} (api {}) on {}/{}",
                host.address,
                v.server_version.as_deref().unwrap_or("?"),
                v.api_version.as_deref().unwrap_or("?"),
                v.os.as_deref().unwrap_or("?"),
                v.arch.as_deref().unwrap_or("?"),
            ),
            Err(e) => {
                had_error = true;
                eprintln!("✗ {}: {e:#}", host.address);
            }
        }
    }
    if had_error {
        anyhow::bail!("preflight failed for one or more hosts");
    }
    Ok(())
}

/// Poll `ops.version(host)` until it returns Ok or the deadline lapses.
/// `wait = None` is one-shot. Returns the last error on timeout so the
/// caller can surface what kept the host unreachable.
async fn probe_host_with_wait(
    ops: &dyn DockerOps,
    host: &Host,
    wait: Option<Duration>,
) -> Result<yoink::docker_ops::DockerVersion> {
    let Some(budget) = wait else {
        return ops.version(host).await.map_err(anyhow::Error::from);
    };
    let deadline = std::time::Instant::now() + budget;
    let mut delay = Duration::from_secs(2);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match ops.version(host).await {
            Ok(v) => {
                if attempt > 1 {
                    eprintln!(
                        "… {}: docker reachable after {attempt} attempt(s)",
                        host.address
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(anyhow::Error::from(e).context(format!(
                        "host {} unreachable after {attempt} attempt(s) within {budget:?}",
                        host.address
                    )));
                }
                let remaining = deadline.saturating_duration_since(now);
                let sleep_for = delay.min(remaining);
                eprintln!(
                    "… {}: not reachable yet (attempt {attempt}); retrying in {sleep_for:?}",
                    host.address
                );
                tokio::time::sleep(sleep_for).await;
                delay = (delay * 2).min(Duration::from_secs(15));
            }
        }
    }
}

// Each bool is a discrete CLI flag; collapsing them would just hide
// the same surface area behind an enum.
#[allow(clippy::struct_excessive_bools)]
struct UpOptions<'a> {
    services: &'a [String],
    tag_args: &'a [String],
    allow_dirty: bool,
    dry_run: bool,
    format: DryRunFormat,
    no_registry: bool,
    transport: yoink::transport::Transport,
    build: bool,
    /// Force a fresh xcaddy build even when `image_present` would
    /// short-circuit. Threaded through to `prefetch_images` →
    /// `ensure_xcaddy_image`. No-op when `proxy.xcaddy:` is unset.
    rebuild_proxy: bool,
    force: bool,
    /// Pin every selected service to the current git short SHA.
    here: bool,
    /// Friendlier alias for `dry_run` — same machinery, different name.
    plan: bool,
    /// Re-reconcile on config-file change. Drives the watch loop in
    /// `cmd_up` after the initial run completes.
    watch: bool,
    /// Source path for the watch loop's `Config::load_from_path` polling.
    /// Always set when `watch` is true; ignored otherwise.
    config_path: &'a std::path::Path,
}

async fn cmd_up(config: &Config, up: UpOptions<'_>) -> Result<()> {
    let dry_run = up.dry_run || up.plan;

    do_up_once(config, &up, dry_run).await?;
    if !up.watch {
        return Ok(());
    }
    if dry_run {
        // The watch loop only makes sense when we're actually
        // mutating — otherwise it would re-print the same diff every
        // 2s. Reject rather than silently spin.
        anyhow::bail!("--watch and --plan/--dry-run are mutually exclusive");
    }

    eprintln!(
        "● watching {} for changes (Ctrl-C to exit)",
        up.config_path.display()
    );
    let mut last_config = config.clone();
    let mut last_error: Option<String> = None;
    let mut tick = tokio::time::interval(WATCH_TICK);
    tick.tick().await; // burn the immediate first fire
    let ctrlc = tokio::signal::ctrl_c();
    tokio::pin!(ctrlc);
    loop {
        tokio::select! {
            _ = &mut ctrlc => {
                eprintln!("\n✗ watch interrupted");
                break;
            }
            _ = tick.tick() => {
                let next = match Config::load_from_path(up.config_path) {
                    Ok(c) => {
                        // Surface the recovery once so the operator
                        // knows yoink is happy again.
                        if last_error.is_some() {
                            eprintln!("✓ config OK — resuming");
                            last_error = None;
                        }
                        c
                    }
                    Err(e) => {
                        // Print the parse error once per *unique*
                        // message — a half-saved edit shouldn't fill
                        // the terminal with the same error every 2s.
                        let msg = format!("✗ config reload failed: {e}");
                        if last_error.as_deref() != Some(msg.as_str()) {
                            eprintln!("{msg}");
                            last_error = Some(msg);
                        }
                        continue;
                    }
                };
                if next == last_config {
                    continue;
                }
                eprintln!("● config changed — reconciling");
                if let Err(e) = do_up_once(&next, &up, dry_run).await {
                    eprintln!("✗ reconcile failed: {e:#}");
                }
                last_config = next;
            }
        }
    }
    Ok(())
}

const WATCH_TICK: std::time::Duration = std::time::Duration::from_secs(2);

#[allow(clippy::too_many_lines)] // single linear up-once flow; splitting fragments the build → push → reconcile sequence
async fn do_up_once(config: &Config, up: &UpOptions<'_>, dry_run: bool) -> Result<()> {
    use yoink::docker_ops::Host;
    use yoink::lock::HostLock;
    let &UpOptions {
        services,
        tag_args,
        allow_dirty,
        format,
        no_registry,
        transport,
        build,
        rebuild_proxy: _,
        force,
        here,
        // `dry_run` arrives as a separate parameter (collapsed with
        // `plan` upstream); `plan`, `watch`, `config_path` are
        // handled by the caller. `rebuild_proxy` is consumed via
        // `up.rebuild_proxy` further down inside `prefetch_images`.
        dry_run: _,
        plan: _,
        watch: _,
        config_path: _,
    } = up;

    // Load bundle first so build_real_ops can reuse it for any
    // per-host ssh_key_secret resolution.
    let bundle = load_secrets_bundle(config).await?;
    // Wrap in Arc so the heartbeat tasks (one per host lock) can hold
    // their own clone for the duration of the deploy.
    let ops: std::sync::Arc<dyn DockerOps> =
        std::sync::Arc::new(build_real_ops(config, bundle.as_ref()).await?);
    // `--here`: resolve current git short SHA and inject as per-service
    // `name=tag` overrides so the rest of the pipeline treats it like
    // any other explicit tag — and `parse_tag_overrides`'s bare-tag-
    // without-service guard doesn't fire.
    let here_overrides: Vec<String> = if here {
        let cwd = std::env::current_dir().context("--here: read current directory")?;
        let sha = git::current_short_sha(&cwd)
            .context("--here: resolve git short SHA from current directory")?;
        let names: Vec<&str> = if services.is_empty() {
            config.services.iter().map(|s| s.name.as_str()).collect()
        } else {
            services.iter().map(String::as_str).collect()
        };
        names.iter().map(|n| format!("{n}={sha}")).collect()
    } else {
        Vec::new()
    };
    let tag_args: Vec<String> = tag_args
        .iter()
        .chain(here_overrides.iter())
        .cloned()
        .collect();
    let tag_overrides = parse_tag_overrides(&tag_args, services, allow_dirty)?;

    let services_filter = services_filter(services);

    if dry_run {
        return run_dry_run(
            ops.as_ref(),
            config,
            &tag_overrides,
            services_filter,
            bundle.as_ref(),
            format,
        )
        .await;
    }

    // Optional `--build` pre-flight: rebuild any selected service that
    // declares a `build:` block before deploying. Lets the indie
    // "drop yoink.yaml in repo and `yoink up --build --no-registry`"
    // loop work as a one-shot without remembering to run `yoink build`
    // separately. Push is intentionally not auto-engaged — kamal-style
    // flows still go through the explicit `yoink build --push` step.
    if build {
        let buildable: Vec<&yoink::config::ServiceConfig> = config
            .selected_services(services_filter)
            .filter(|s| s.build.is_some())
            .collect();
        if buildable.is_empty() {
            // Silent no-op was the worst UX — operator passes --build,
            // assumes building happened, deploys an unchanged image.
            // Loud-fail with the diagnosis.
            let selected: Vec<&str> = config
                .selected_services(services_filter)
                .map(|s| s.name.as_str())
                .collect();
            anyhow::bail!(
                "--build was set but no selected service has a `build:` block. \
                 Selected: [{}]. Add `build: {{ context: . }}` to a service, \
                 or drop --build for an image-only deploy.",
                selected.join(", ")
            );
        }
        for svc in buildable {
            let tag = yoink::build::resolve_service_tag(svc, &tag_overrides)?;
            yoink::build::build_service(config, svc, &tag, false, false)
                .await
                .with_context(|| format!("build {}", svc.name))?;
        }
    }

    // Plain `yoink up` should work for every config shape — pure
    // registry, pure local-build, or mixed. Two independent shipping
    // paths cover every case:
    //
    //   1. `load_images_to_hosts` ships images from the operator's
    //      local docker daemon. Always run when at least one service
    //      has a `build:` block (those images live only locally).
    //      `--no-registry` widens the filter to ship every selected
    //      service (not just buildable ones) — useful for airgapped /
    //      offline deploys.
    //
    //   2. `prefetch_images` (below, after host locks) pulls registry-
    //      hosted images on each host. Skipped when `--no-registry` is
    //      set or when no service requires a pull (every service has a
    //      `build:` block, so there is nothing to pull). Always skips
    //      `build:` services internally — they're never in any
    //      registry by definition.
    let has_buildable = config.services.iter().any(|s| s.build.is_some());
    let needs_pull = config.any_service_requires_pull();
    if has_buildable || no_registry {
        yoink::build::load_images_to_hosts(
            ops.as_ref(),
            config,
            &tag_overrides,
            services_filter,
            transport,
            !no_registry,
        )
        .await?;
    }

    // Per-host advisory locks. Sentinel container holds the lock; a
    // tokio heartbeat task touches it every few seconds. If yoink
    // dies, heartbeats stop, sentinel self-exits within ~30s, and
    // the next deploy reaps it as an orphan. See `lock.rs`.
    let acquire_futs = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let ops = ops.clone();
        async move {
            let lock = HostLock::acquire(&*ops, host.clone())
                .await
                .with_context(|| format!(
                    "another `yoink up` appears to be in progress on {} (lock container running). \
                     If you're sure no operator is deploying, the sentinel will self-exit within ~30s; \
                     retry then. To force-clear: `docker rm -f yoink-deploy-lock` on the host.",
                    host.address,
                ))?;
            anyhow::Ok(lock)
        }
    });
    let mut locks: Vec<HostLock> = futures_util::future::try_join_all(acquire_futs).await?;
    for lock in &mut locks {
        lock.spawn_heartbeat(ops.clone());
    }

    let mut sink = |service: Option<&str>, event: deploy::DeployEvent| {
        eprintln!("{}", output::format_deploy_event(service, &event));
    };

    // Phase 0: prefetch all registry-hosted service images in parallel
    // when this is a "deploy everything" run. Single-service deploys
    // would gain nothing (one image to pull) so skip the wrapper.
    // Skipped entirely when `--no-registry` is set (the operator chose
    // local-only mode) or when no service requires a pull (every
    // service has a `build:` block — already shipped above).
    if services_filter.is_none() && needs_pull && !no_registry {
        let prefetch_cb: std::sync::Arc<dyn Fn(deploy::DeployEvent) + Send + Sync> =
            std::sync::Arc::new(|e| {
                eprintln!("{}", output::format_deploy_event(None, &e));
            });
        if let Err(e) = deploy::prefetch_images(
            ops.clone(),
            config,
            &tag_overrides,
            services_filter,
            bundle.as_ref(),
            true,
            up.rebuild_proxy,
            prefetch_cb,
        )
        .await
        {
            for lock in locks {
                lock.release(&*ops).await;
            }
            return Err(e).context("prefetch images");
        }
    }

    let reconcile_result = deploy::reconcile_with_options(
        &*ops,
        config,
        &tag_overrides,
        services_filter,
        bundle.as_ref(),
        deploy::ReconcileOptions { force },
        &mut sink,
    )
    .await;

    // Always release the locks, even on reconcile failure — keeping
    // them around blocks the operator's next attempt until the
    // sentinel self-exits.
    for lock in locks {
        lock.release(&*ops).await;
    }

    let reports = reconcile_result.context("reconcile")?;
    eprintln!("{}", output::format_deploy_summary(&reports));
    Ok(())
}

/// Convert the CLI's `--service` repeat into the `Option<&[String]>`
/// shape every downstream caller wants: empty list → no filter →
/// "every service"; non-empty → filter to those.
fn services_filter(services: &[String]) -> Option<&[String]> {
    if services.is_empty() {
        None
    } else {
        Some(services)
    }
}

/// Build the `service_name → tag` override map. Each `--tag` argument
/// is either `name=tag` (override that named service's tag) or a bare
/// `tag` (applies to every `--service`-selected service). A bare tag
/// without `--service` is rejected as ambiguous in multi-service mode.
fn parse_tag_overrides(
    tag_args: &[String],
    services_filter: &[String],
    allow_dirty: bool,
) -> Result<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;
    let mut out = BTreeMap::new();
    let mut bare: Option<String> = None;
    for arg in tag_args {
        if let Some((name, tag)) = arg.split_once('=') {
            out.insert(name.to_string(), tag.to_string());
        } else if bare.is_some() {
            anyhow::bail!("multiple bare --tag values; use --tag <name>=<tag> per service");
        } else {
            bare = Some(arg.clone());
        }
    }
    if let Some(t) = bare {
        if services_filter.is_empty() {
            // No service filter: bare tag is treated as "current commit"
            // applied to every service in the config that uses git
            // versioning. We don't know which from here, so the user is
            // expected to pair `--tag` with `--service` for now.
            anyhow::bail!(
                "bare --tag without --service is ambiguous in multi-service mode; \
                 use --service <name> --tag <sha> per service or --tag <name>=<tag>"
            );
        }
        for service in services_filter {
            out.insert(service.clone(), t.clone());
        }
    } else if !out.is_empty() {
        // user provided per-service tags only; nothing else to do.
    } else if !services_filter.is_empty() && !allow_dirty {
        // fall back to git version for selected services. Helpful in CI
        // where you'd otherwise just `--tag $GIT_SHA`. Operator can
        // always opt out by passing tags explicitly.
        if let Ok(version) = git::version(std::path::Path::new("."), allow_dirty) {
            for service in services_filter {
                out.insert(service.clone(), version.clone());
            }
        }
    }
    Ok(out)
}

struct BuildOptions<'a> {
    services: &'a [String],
    tag_args: &'a [String],
    allow_dirty: bool,
    no_cache: bool,
    push: bool,
}

/// `yoink build` — run `docker build` for every selected service that
/// declares a `build:` block. Tags the result on the operator's
/// local daemon as `<image>:<tag>`, ready for `yoink up --no-registry`.
async fn cmd_build(config: &Config, build: BuildOptions<'_>) -> Result<()> {
    let BuildOptions {
        services,
        tag_args,
        allow_dirty,
        no_cache,
        push,
    } = build;
    let tag_overrides = parse_tag_overrides(tag_args, services, allow_dirty)?;
    let services_filter = services_filter(services);
    let explicit_services = services_filter.is_some();

    let mut built_any = false;
    for svc in config.selected_services(services_filter) {
        if svc.build.is_none() {
            // Explicit `--service api` against a service without a
            // `build:` block is operator error — they expected a
            // build. Bare `yoink build` (no filter) just skips
            // registry-only services silently.
            if explicit_services {
                anyhow::bail!(
                    "service {:?} has no `build:` block — add one or build the image yourself",
                    svc.name
                );
            }
            continue;
        }
        let tag = yoink::build::resolve_service_tag(svc, &tag_overrides)?;
        yoink::build::build_service(config, svc, &tag, no_cache, push)
            .await
            .with_context(|| format!("build {}", svc.name))?;
        built_any = true;
    }
    if !built_any {
        anyhow::bail!(
            "no services with a `build:` block matched. \
             Add `build:` to a service or pass --service to one that has it."
        );
    }
    Ok(())
}

async fn cmd_status(config: &Config, json: bool) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status")?;
    if json {
        // serde_json::to_string_pretty over StatusReport (derives Serialize on
        // each level) — stable contract for CI parsing.
        let out = serde_json::to_string_pretty(&report).context("serialize status as json")?;
        println!("{out}");
    } else {
        println!("{}", output::format_status_table(&report));
    }
    Ok(())
}

async fn cmd_rollback(config: &Config, service: String, tag: Option<String>) -> Result<()> {
    use yoink::docker_ops::Host;
    let ops = build_real_ops(config, None).await?;

    // Verify the service exists in config; otherwise the user typo'd.
    if !config.services.iter().any(|s| s.name == service) {
        anyhow::bail!(
            "service {service:?} is not declared in the loaded config; \
             pass --service to one that is"
        );
    }

    // Resolve the tag — either user-supplied or discovered.
    let resolved_tag = if let Some(t) = tag {
        t
    } else {
        // Walk every configured host, gather containers labeled with this
        // service, find the newest non-running one. That's the tag we
        // rolled away from. Skip the currently-running container even if
        // its `created_unix` would otherwise win.
        let label = format!("yoink.service={service}");
        let mut candidates: Vec<(i64, String)> = Vec::new();
        let mut current_versions: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for host_cfg in &config.hosts {
            let host = Host::from(host_cfg);
            let containers = ops
                .list_containers_by_label(&host, &label)
                .await
                .with_context(|| format!("list {label} on {}", host.address))?;
            for c in containers {
                if let Some(v) = &c.yoink_version {
                    if c.is_running() {
                        current_versions.insert(v.clone());
                    } else if let Some(ts) = c.created_unix {
                        candidates.push((ts, v.clone()));
                    }
                }
            }
        }
        candidates.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
        candidates
            .into_iter()
            .find(|(_, v)| !current_versions.contains(v))
            .map(|(_, v)| v)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no previous version of {service:?} found on any host; \
                     pass --tag explicitly to deploy a specific image"
                )
            })?
    };

    info_eprintln!("rolling {service} back to tag {resolved_tag}");
    let services_arg = std::slice::from_ref(&service);
    let tag_arg = format!("{service}={resolved_tag}");
    let tag_args = std::slice::from_ref(&tag_arg);
    cmd_up(
        config,
        UpOptions {
            services: services_arg,
            tag_args,
            allow_dirty: true,
            dry_run: false,
            format: DryRunFormat::Text,
            no_registry: false,
            transport: yoink::transport::Transport::Auto,
            build: false,
            rebuild_proxy: false,
            force: false,
            here: false,
            plan: false,
            watch: false,
            config_path: std::path::Path::new(""),
        },
    )
    .await
}

async fn cmd_prune(config: &Config, dry_run: bool) -> Result<()> {
    use yoink::prune::{self, PruneReason};
    let ops = build_real_ops(config, None).await?;
    let report = prune::run(&ops, config, dry_run).await.context("prune")?;
    let items = if dry_run {
        &report.planned
    } else {
        &report.removed
    };
    if items.is_empty() {
        eprintln!("nothing to prune");
        return Ok(());
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for item in items {
        let svc = item.service.as_deref().unwrap_or("?");
        let reason = match item.reason {
            PruneReason::ServiceNotInConfig => "service not in config",
            PruneReason::StaleExited => "stale exited",
        };
        println!(
            "[{}] {verb} {} (service={svc}, {reason})",
            item.host, item.container
        );
    }
    Ok(())
}

async fn load_secrets_bundle(config: &Config) -> Result<Option<SecretsBundle>> {
    secrets::load_bundle(config)
        .await
        .context("load secrets bundle")
}

/// Build a `RealDockerOps` aware of any `hosts[].ssh_key_secret`
/// entries — decrypts them into 0600 tempfiles held inside the ops
/// object and threads the paths into bollard + `ssh_probe`.
///
/// `bundle` is the caller's pre-loaded secrets bundle (e.g. `cmd_up`
/// already loads it for service-level secrets). Pass `None` when the
/// caller doesn't otherwise need the bundle — it'll be loaded on
/// demand only if a host actually declares `ssh_key_secret:`.
async fn build_real_ops(config: &Config, bundle: Option<&SecretsBundle>) -> Result<RealDockerOps> {
    // Fast path: no host needs a managed key. Skip bundle access
    // entirely so commands that don't otherwise touch secrets pay
    // nothing.
    if config.hosts.iter().all(|h| h.ssh_key_secret.is_none()) {
        return Ok(RealDockerOps::new());
    }
    let owned_bundle;
    let bundle = if let Some(b) = bundle {
        Some(b)
    } else {
        owned_bundle = load_secrets_bundle(config).await?;
        owned_bundle.as_ref()
    };
    let km = yoink::ssh_keys::prepare(config, bundle).context("prepare per-host ssh keys")?;
    Ok(RealDockerOps::with_key_manager(km.map(std::sync::Arc::new)))
}

/// Resolve `(service, optional host)` to exactly one running container
/// via the `yoink.service=<name>` label. Errors with the candidate set
/// when the user doesn't pin a host on a multi-replica service, so an
/// `exec` or `logs` command can't accidentally hit the wrong replica.
/// Every running replica of `service` (optionally filtered to a host).
/// Used by both the strict `resolve_running_container` (errors on >1)
/// and the replica-aware shell/logs paths (auto-pick / multiplex).
/// Per-host listings run in parallel — for a fleet of N hosts this is
/// one round-trip instead of N. Same fan-out shape used elsewhere in
/// the CLI (deploy.rs, prune, prefetch).
async fn list_running_replicas(
    ops: &dyn DockerOps,
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
) -> Result<Vec<(yoink::docker_ops::Host, yoink::docker_ops::ContainerInfo)>> {
    use yoink::docker_ops::Host;
    let label = format!("yoink.service={service}");
    let futs = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|host_cfg| {
            let host = Host::from(host_cfg);
            let label = &label;
            async move {
                let containers = ops
                    .list_containers_by_label(&host, label)
                    .await
                    .with_context(|| format!("list containers on {}", host.address))?;
                anyhow::Ok((host, containers))
            }
        });
    let per_host = futures_util::future::try_join_all(futs).await?;
    let mut candidates = Vec::new();
    for (host, containers) in per_host {
        for c in containers {
            if c.is_running() {
                candidates.push((host.clone(), c));
            }
        }
    }
    Ok(candidates)
}

/// Strict resolver — errors on zero or many candidates. Used by gestures
/// that target *one* container (`exec`, `restart`, `version`, `logs`
/// without multiplexing).
async fn resolve_running_container(
    ops: &dyn DockerOps,
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
) -> Result<(yoink::docker_ops::Host, String)> {
    let candidates = list_running_replicas(ops, config, service, host_filter).await?;
    match candidates.len() {
        0 => Err(no_replicas_err(service, host_filter)),
        1 => {
            let (h, c) = candidates.into_iter().next().expect("len == 1");
            Ok((h, c.name))
        }
        _ => {
            let listing = candidates
                .iter()
                .map(|(h, c)| format!("  {} → {}", h.address, c.name))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "service {service:?} has multiple replicas; pin one with --host:\n{listing}"
            )
        }
    }
}

/// Standard "no running replicas" error, shared by every gesture
/// that targets a service by name (`shell`, `logs`, `exec`, …).
fn no_replicas_err(service: &str, host_filter: Option<&str>) -> anyhow::Error {
    anyhow::anyhow!(
        "no running container with yoink.service={service}{}",
        host_filter
            .map(|h| format!(" on host {h}"))
            .unwrap_or_default()
    )
}

/// Replica-aware picker: prefer the first explicitly healthy replica;
/// fall back to the first running. The fall-back covers services with
/// no `healthcheck_path:` (where `health_hint()` is always `None`) so
/// `yoink shell <svc>` still works on them.
fn pick_healthy_replica(
    candidates: Vec<(yoink::docker_ops::Host, yoink::docker_ops::ContainerInfo)>,
) -> Option<(yoink::docker_ops::Host, yoink::docker_ops::ContainerInfo)> {
    if let Some(idx) = candidates
        .iter()
        .position(|(_, c)| c.health_hint() == Some("healthy"))
    {
        return candidates.into_iter().nth(idx);
    }
    candidates.into_iter().next()
}

async fn cmd_exec(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    cmd: Vec<String>,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    let result = ops
        .exec_oneshot(&host, &container, cmd)
        .await
        .with_context(|| format!("exec in {}@{container}", host.address))?;
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    if result.exit_code != 0 {
        anyhow::bail!("command exited with code {}", result.exit_code);
    }
    Ok(())
}

async fn cmd_logs(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    follow: bool,
    tail: u32,
) -> Result<()> {
    let ops: std::sync::Arc<dyn DockerOps> =
        std::sync::Arc::new(build_real_ops(config, None).await?);
    let candidates = list_running_replicas(ops.as_ref(), config, service, host_filter).await?;
    if candidates.is_empty() {
        return Err(no_replicas_err(service, host_filter));
    }
    if follow {
        cmd_logs_follow(ops, candidates, tail).await
    } else {
        cmd_logs_tail(&*ops, candidates, tail).await
    }
}

/// `[host/container] ` prefix, or empty when there's only one replica
/// (preserved so shell pipelines piping `yoink logs` into `grep` keep
/// working unchanged for the single-replica case).
fn replica_prefix(
    host: &yoink::docker_ops::Host,
    info: &yoink::docker_ops::ContainerInfo,
    multi: bool,
) -> String {
    if multi {
        format!("[{}/{}] ", host.address, info.name)
    } else {
        String::new()
    }
}

/// Multiplexed `--follow` path: one stream task per replica fanned
/// into a shared mpsc. `drop(tx)` after the spawn loop closes the
/// channel once every per-replica task exits.
async fn cmd_logs_follow(
    ops: std::sync::Arc<dyn DockerOps>,
    candidates: Vec<(yoink::docker_ops::Host, yoink::docker_ops::ContainerInfo)>,
    tail: u32,
) -> Result<()> {
    let multi = candidates.len() > 1;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for (host, info) in candidates {
        let ops = ops.clone();
        let tx = tx.clone();
        let prefix = replica_prefix(&host, &info, multi);
        tokio::spawn(async move {
            let mut stream = match ops.open_log_stream(&host, &info.name, tail).await {
                Ok(rx) => rx,
                Err(e) => {
                    let _ = tx.send(format!("{prefix}log stream failed: {e}"));
                    return;
                }
            };
            while let Some(line) = stream.recv().await {
                let formatted = if prefix.is_empty() {
                    line.message
                } else {
                    format!("{prefix}{}", line.message)
                };
                if tx.send(formatted).is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);
    let ctrlc = tokio::signal::ctrl_c();
    tokio::pin!(ctrlc);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(s) => println!("{s}"),
                None => break,
            },
            _ = &mut ctrlc => break,
        }
    }
    Ok(())
}

/// One-shot tail path: fetch each replica's recent log buffer and
/// print, prefixing per-line when there's >1 replica. `fetch_recent_logs`
/// returns lines with trailing newlines preserved, so prefixes are
/// inserted before each `\n`-delimited segment.
async fn cmd_logs_tail(
    ops: &dyn DockerOps,
    candidates: Vec<(yoink::docker_ops::Host, yoink::docker_ops::ContainerInfo)>,
    tail: u32,
) -> Result<()> {
    let multi = candidates.len() > 1;
    for (host, info) in candidates {
        let lines = ops
            .fetch_recent_logs(&host, &info.name, tail)
            .await
            .with_context(|| format!("fetch logs {}@{}", host.address, info.name))?;
        let prefix = replica_prefix(&host, &info, multi);
        for l in lines {
            if prefix.is_empty() {
                print!("{l}");
            } else {
                for raw in l.split_inclusive('\n') {
                    print!("{prefix}{raw}");
                }
            }
        }
    }
    Ok(())
}

async fn cmd_version(config: &Config, service: &str) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect_for_service(&ops, config, service)
        .await
        .context("collect status")?;
    let mut printed = false;
    for host in &report.hosts {
        for c in &host.containers {
            if !c.is_running() {
                continue;
            }
            let version = c.yoink_version.as_deref().unwrap_or("?");
            println!("{}: {} → {} ({})", host.host, c.name, version, c.state);
            printed = true;
        }
    }
    if !printed {
        anyhow::bail!("no running container for service {service:?}");
    }
    Ok(())
}

/// Which flavor of PTY session `cmd_pty` opens — same byte-pump
/// otherwise, the `Debug` arm just exec's an alpine sidecar in the
/// target's pid+net namespaces (and gets force-removed on exit).
enum PtyMode {
    Exec,
    Debug { image: String },
}

/// `yoink pf` — bind a laptop port to a container port. Foreground;
/// holds the tunnel until SIGINT. Picks the published-port fast path
/// when available (`auto` mode), or spawns a socat sidecar for
/// services that don't expose host ports — the secure-by-default
/// shape (api/web behind Caddy in production).
#[allow(clippy::fn_params_excessive_bools)] // operator-facing flags, explicit at the CLI; bundling into a struct hides them.
#[allow(clippy::too_many_arguments)] // mirrors the CLI surface 1:1; struct would force a noop builder.
async fn cmd_pf(
    config: &Config,
    service_name: &str,
    port_arg: Option<&str>,
    host_filter: Option<&str>,
    replica: usize,
    open_browser: bool,
    scheme_override: yoink::pf::SchemeOverride,
    json: bool,
    mode: yoink::pf::Mode,
) -> Result<()> {
    use yoink::pf;
    use yoink::transport::tunnel::SshTunnel;

    let service = pf::resolve_service(config, service_name)?;

    // Resolve `(LOCAL, CONTAINER)`. The container port comes from
    // either an explicit arg, the unique publish (when there is
    // exactly one), or service.run.port (the healthcheck port,
    // which is the right default for non-published services).
    let (local_port_request, container_port) = match port_arg {
        Some(arg) => parse_pf_port_arg(arg)?,
        None => {
            if let Some(ep) = pf::sole_publish(service) {
                (Some(ep.host_port), ep.container_port)
            } else if let Some(p) = service.run.port {
                (None, p)
            } else {
                anyhow::bail!(
                    "service {service_name:?} has no `publish:` and no `run.port:` — \
                     specify the container port explicitly, e.g. `yoink pf {service_name} 8080`"
                );
            }
        }
    };

    // Pick the host: respect --host if given; otherwise the first
    // host the service applies to. Reject if zero applicable hosts.
    let applicable: Vec<_> = service
        .applicable_hosts(&config.hosts)
        .into_iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .collect();
    let host_cfg = match applicable.as_slice() {
        [] => anyhow::bail!(
            "service {service_name:?} has no applicable host{}",
            host_filter.map_or(String::new(), |f| format!(" matching --host={f}")),
        ),
        [single] => *single,
        many if host_filter.is_some() => many[0],
        many => anyhow::bail!(
            "service {service_name:?} runs on {} hosts; pin one with --host:\n{}",
            many.len(),
            many.iter()
                .map(|h| format!("  {}", h.address))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };

    if u32::try_from(replica).map_or(true, |r| r >= service.run.replicas) {
        anyhow::bail!(
            "service {service_name:?} has {} replica(s); --replica {replica} is out of range",
            service.run.replicas,
        );
    }

    let host = yoink::docker_ops::Host::from(host_cfg);
    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(build_real_ops(config, None).await?);
    let keyfile = ops.ssh_keyfile(&host);
    yoink::ssh_probe::probe(&host, keyfile.as_deref().and_then(|p| p.to_str()))
        .await
        .map_err(|e| anyhow::anyhow!("ssh probe to {}: {e}", host.address))?;

    // Resolve the path: published host endpoint OR sidecar handle.
    // `_sidecar` is bound here so its Drop fires after SIGINT even
    // though the variable is otherwise unused.
    let resolved = pf::resolve_target(ops.clone(), &host, service, container_port, mode).await?;
    let (remote_dial_host, remote_port, mode_label, _sidecar) = match resolved {
        pf::ResolvedTarget::Published(ep) => (ep.host_ip, ep.host_port, "published", None),
        pf::ResolvedTarget::Sidecar(handle) => {
            let port = handle.host_port();
            (
                pf::SIDECAR_DIAL_HOST.to_string(),
                port,
                "sidecar",
                Some(handle),
            )
        }
    };

    let tunnel = SshTunnel::open_with_local_port(
        &host.user,
        &host.address,
        &remote_dial_host,
        remote_port,
        local_port_request,
        pf::TUNNEL_READY_TIMEOUT,
        keyfile.as_deref(),
    )
    .await
    .with_context(|| {
        format!(
            "open ssh tunnel to {}:{remote_dial_host}:{remote_port}",
            host.address
        )
    })?;

    let local_port = tunnel.local_port();
    let url = pf::forward_url(local_port, container_port, scheme_override);

    if json {
        let line = serde_json::json!({
            "service": service_name,
            "host": host.address,
            "mode": mode_label,
            "remote_ip": remote_dial_host,
            "remote_port": remote_port,
            "container_port": container_port,
            "local_port": local_port,
            "url": url,
        });
        println!("{line}");
    } else {
        eprintln!(
            "→ {} ({} via {mode_label}) ⇆ {url}\n  Ctrl-C to close",
            service_name, host.address
        );
    }

    if open_browser && let Err(e) = pf::open_in_browser(&url) {
        eprintln!("✗ failed to open browser: {e}\n  paste into one yourself: {url}");
    }

    // Hold open until SIGINT. Order matters: close the sidecar
    // FIRST (the bollard SSH connection is still live and warm),
    // THEN drop the SshTunnel (synchronously kills the ssh -L
    // child). Reverse order saw bollard return SendRequest errors
    // on the docker remove call, presumably because something in
    // the tunnel teardown was poking the same SSH stack bollard
    // uses.
    tokio::signal::ctrl_c()
        .await
        .context("install SIGINT handler")?;
    if let Some(sc) = _sidecar {
        sc.close().await;
    }
    drop(tunnel);
    if !json {
        eprintln!("\n✓ tunnel closed");
    }
    Ok(())
}

fn parse_humantime(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration {s:?}: {e}"))
}

/// Parse `LOCAL:CONTAINER` or just `CONTAINER`. Empty / non-numeric
/// segments reject with a stable message for shell-completion friendliness.
fn parse_pf_port_arg(arg: &str) -> Result<(Option<u16>, u16)> {
    let (local, container) = match arg.split_once(':') {
        Some((l, c)) => (Some(l), c),
        None => (None, arg),
    };
    let container: u16 = container
        .parse()
        .with_context(|| format!("container port {container:?} not in 0..=65535"))?;
    let local = match local {
        Some(s) => Some(
            s.parse::<u16>()
                .with_context(|| format!("local port {s:?} not in 0..=65535"))?,
        ),
        None => Some(container),
    };
    Ok((local, container))
}

async fn cmd_pty(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    mode: PtyMode,
) -> Result<()> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

    let ops: std::sync::Arc<dyn DockerOps> =
        std::sync::Arc::new(build_real_ops(config, None).await?);
    // For shell, we don't need a single-replica guarantee — pick a
    // healthy replica (or the first running one) and tell the operator
    // which one we landed on.
    let candidates = list_running_replicas(ops.as_ref(), config, service, host_filter).await?;
    if candidates.is_empty() {
        return Err(no_replicas_err(service, host_filter));
    }
    let multi = candidates.len() > 1;
    let (host, info) = pick_healthy_replica(candidates).expect("non-empty checked above");
    let container = info.name.clone();
    if multi {
        eprintln!(
            "→ {}/{} ({})",
            host.address,
            container,
            info.health_hint().unwrap_or("running"),
        );
    }

    let (cols, rows) = size().context("query terminal size")?;

    enable_raw_mode().context("enable raw mode")?;
    let result = pty_session(ops.clone(), &host, &container, &mode, rows, cols).await;
    let _ = disable_raw_mode();
    // Newline so the operator's next shell prompt isn't glued to the
    // last line of in-container output.
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\r\n");
    let _ = stdout.flush();
    result
}

async fn pty_session(
    ops: std::sync::Arc<dyn DockerOps>,
    host: &Host,
    container: &str,
    mode: &PtyMode,
    rows: u16,
    cols: u16,
) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use yoink::docker_ops::ExecKind;

    let session = match mode {
        PtyMode::Exec => {
            let cmd = vec![
                "/bin/sh".into(),
                "-c".into(),
                "if command -v bash >/dev/null 2>&1; then exec bash; else exec /bin/sh; fi".into(),
            ];
            ops.exec_interactive(host, container, cmd, rows, cols)
                .await
                .with_context(|| format!("exec interactive {}@{container}", host.address))?
        }
        PtyMode::Debug { image } => ops
            .start_debug_sidecar(host, container, image, rows, cols)
            .await
            .with_context(|| format!("start debug sidecar on {}", host.address))?,
    };

    let session_id = session.id.clone();
    let session_kind = session.kind;
    let yoink::docker_ops::ExecSession {
        mut stdin,
        mut output,
        ..
    } = session;

    // Pump exec output → terminal stdout. Runs in a background task so
    // the main task can pump stdin → exec.stdin concurrently. Returns
    // when the docker stream EOFs (container shell exited).
    let output_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(item) = output.next().await {
            match item {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    if stdout.write_all(&bytes).await.is_err() {
                        return;
                    }
                    let _ = stdout.flush().await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "exec output stream error");
                    return;
                }
            }
        }
    });

    // Pump terminal stdin → exec.stdin. We treat stdin as raw bytes
    // (the terminal is in raw mode already, so each keypress is the
    // exact byte sequence the in-container shell expects).
    let stdin_task = tokio::spawn(async move {
        let mut stdin_in = tokio::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin_in.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if stdin.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                    let _ = stdin.flush().await;
                }
            }
        }
    });

    // SIGWINCH → resize_exec / resize_container_tty. macOS + Linux
    // both have it; the docker daemon needs the new size so apps
    // like `top` and `vim` re-flow.
    let resize_ops = ops.clone();
    let resize_host = host.clone();
    let resize_id = session_id.clone();
    let resize_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut sig) = signal(SignalKind::window_change()) else {
            return;
        };
        while sig.recv().await.is_some() {
            let Ok((cols, rows)) = crossterm::terminal::size() else {
                continue;
            };
            let res = match session_kind {
                ExecKind::Exec => {
                    resize_ops
                        .resize_exec(&resize_host, &resize_id, rows, cols)
                        .await
                }
                ExecKind::Sidecar => {
                    resize_ops
                        .resize_container_tty(&resize_host, &resize_id, rows, cols)
                        .await
                }
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, "resize failed");
            }
        }
    });

    // The output task ending means the container shell exited (Ctrl-D
    // / `exit`). The stdin task only ends when our process's stdin is
    // closed, which won't happen normally — abort it on output EOF.
    let _ = output_task.await;
    stdin_task.abort();
    resize_task.abort();

    // Sidecar cleanup — `auto_remove` should handle it but belt + braces.
    if matches!(session_kind, ExecKind::Sidecar)
        && let Err(e) = ops.force_remove_container(host, &session_id).await
    {
        tracing::debug!(error = %e, "sidecar cleanup (probably already auto-removed)");
    }

    Ok(())
}

async fn cmd_tui(config: &Config, config_path: PathBuf, mode: Mode, mouse: bool) -> Result<()> {
    let mut config = config.clone();
    config.push_local_host_if_socket();
    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(build_real_ops(&config, None).await?);
    tui::run(&config, config_path, ops, mode, mouse)
        .await
        .context("run TUI")
}

/// Output format for `yoink up --dry-run`. Mirrors `diff::Format` but
/// is `clap::ValueEnum`-derivable so the CLI surface stays at the
/// binary boundary.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum DryRunFormat {
    #[default]
    Text,
    Markdown,
    Json,
}

/// Output format for read-only listings (`history`, `top`, …). `text`
/// is the default human-readable table; `json` is a stable schema for
/// pipelines.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum TableFormat {
    #[default]
    Text,
    Json,
}

/// CLI shape for `--transport`; mirrors `yoink::transport::Transport` so
/// the value-enum stays bound to the binary surface.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum TransportMode {
    #[default]
    Auto,
    Unregistry,
    Tarball,
}

impl From<TransportMode> for yoink::transport::Transport {
    fn from(m: TransportMode) -> Self {
        match m {
            TransportMode::Auto => Self::Auto,
            TransportMode::Unregistry => Self::Unregistry,
            TransportMode::Tarball => Self::Tarball,
        }
    }
}

impl From<DryRunFormat> for yoink::diff::Format {
    fn from(f: DryRunFormat) -> Self {
        match f {
            DryRunFormat::Text => Self::Text,
            DryRunFormat::Markdown => Self::Markdown,
            DryRunFormat::Json => Self::Json,
        }
    }
}

async fn run_dry_run(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &std::collections::BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&yoink::secrets::SecretsBundle>,
    format: DryRunFormat,
) -> Result<()> {
    let report = yoink::diff::compute(ops, config, tag_overrides, services_filter, secrets)
        .await
        .context("compute dry-run diff")?;
    print!("{}", report.render(format.into()));
    Ok(())
}

async fn cmd_restart(config: &Config, service: &str, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    let drain = std::time::Duration::from_secs(10);
    info_eprintln!("stopping {}@{container} (drain {drain:?})…", host.address);
    ops.stop_container(&host, &container, drain)
        .await
        .with_context(|| format!("stop {}@{container}", host.address))?;
    info_eprintln!("starting {}@{container}…", host.address);
    ops.start_container(&host, &container)
        .await
        .with_context(|| format!("start {}@{container}", host.address))?;
    info_eprintln!("ok");
    Ok(())
}

async fn cmd_kill(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    yes: bool,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    if !yes {
        confirm_destructive(&format!(
            "about to SIGKILL {}/{container} — the in-process drain is skipped.",
            host.address
        ))?;
    }
    ops.kill_container(&host, &container)
        .await
        .with_context(|| format!("kill {}@{container}", host.address))?;
    info_eprintln!("killed {}/{container}", host.address);
    Ok(())
}

async fn cmd_pull(
    config: &Config,
    service: &str,
    tag_override: Option<&str>,
    host_filter: Option<&str>,
) -> Result<()> {
    let svc_cfg = config
        .services
        .iter()
        .find(|s| s.name == service)
        .with_context(|| format!("service {service:?} not in config"))?;
    let tag = tag_override
        .map(str::to_string)
        .or_else(|| svc_cfg.tag.clone())
        .unwrap_or_else(|| {
            // Bare `git` version — same fallback `up` uses when no tag is given.
            git::version(std::path::Path::new("."), true).unwrap_or_else(|_| "latest".into())
        });
    let bundle = load_secrets_bundle(config).await?;
    let credentials = deploy::registry_credentials(config, bundle.as_ref());
    let ops = build_real_ops(config, bundle.as_ref()).await?;
    // Fan out across hosts so a slow daemon doesn't block the others.
    let pulls = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|host_cfg| {
            let host = Host::from(host_cfg);
            let image = svc_cfg.image.clone();
            let tag = tag.clone();
            let credentials = credentials.clone();
            let ops = &ops;
            async move {
                info_eprintln!(
                    "→ {}: pulling {}",
                    host.address,
                    yoink::docker::image_reference(&image, &tag),
                );
                ops.pull_image(&host, &image, &tag, credentials)
                    .await
                    .with_context(|| format!("pull on {}", host.address))
                    .map(|()| host.address)
            }
        });
    let results = futures_util::future::join_all(pulls).await;
    let mut had_err = false;
    for r in results {
        match r {
            Ok(addr) => eprintln!("✓ {addr}"),
            Err(e) => {
                eprintln!("✗ {e:#}");
                had_err = true;
            }
        }
    }
    if had_err {
        anyhow::bail!("one or more pulls failed");
    }
    Ok(())
}

async fn cmd_history(
    config: &Config,
    service: &str,
    limit: usize,
    format: TableFormat,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let label = format!("yoink.service={service}");
    // Fan out across hosts. Each call returns running + exited
    // containers labeled with this service.
    let probes = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let label = label.clone();
        let ops = &ops;
        async move {
            let containers = ops.list_containers_by_label(&host, &label).await?;
            anyhow::Ok((host.address, containers))
        }
    });
    let mut entries: Vec<(Option<i64>, String, String, String, String, String)> = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        let (host_addr, containers) = r?;
        for c in containers {
            // Sort key = deployed-at when present, else created_unix.
            let when = c.yoink_deployed_at.or(c.created_unix);
            entries.push((
                when,
                host_addr.clone(),
                c.name,
                c.yoink_version.unwrap_or_else(|| "?".into()),
                c.state,
                c.yoink_deployed_by.unwrap_or_else(|| "?".into()),
            ));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0.unwrap_or(0)));
    if entries.is_empty() {
        anyhow::bail!("no yoink-managed containers found for service {service:?}");
    }
    let trimmed: Vec<_> = entries.into_iter().take(limit).collect();

    match format {
        TableFormat::Json => {
            let out: Vec<serde_json::Value> = trimmed
                .iter()
                .map(|(when, host, name, version, state, by)| {
                    serde_json::json!({
                        "host": host,
                        "container": name,
                        "version": version,
                        "state": state,
                        "deployed_by": by,
                        "deployed_at_unix": when,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        TableFormat::Text => {
            use std::fmt::Write as _;
            let mut buf = String::new();
            writeln!(
                buf,
                "{:<22}  {:<28}  {:<10}  {:<10}  {:<10}  when",
                "host", "container", "version", "state", "deployed-by"
            )?;
            for (when, host_addr, name, version, state, by) in trimmed {
                let when_str = match when {
                    Some(t) if t > 0 => output::format_relative_time(Some(t)),
                    _ => "?".into(),
                };
                writeln!(
                    buf,
                    "{host_addr:<22}  {name:<28}  {version:<10}  {state:<10}  {by:<10}  {when_str}"
                )?;
            }
            page_output(&buf);
        }
    }
    Ok(())
}

struct TopRow {
    cpu_pct: f64,
    host: String,
    service: String,
    container: String,
    mem_used: i64,
    mem_limit: Option<i64>,
    created: Option<i64>,
}

async fn cmd_top(config: &Config, limit: usize, format: TableFormat) -> Result<()> {
    use yoink::output::{format_bytes, format_relative_time};
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status")?;

    let mut targets: Vec<(String, String)> = Vec::new();
    for h in &report.hosts {
        for c in &h.containers {
            if c.is_running() {
                targets.push((h.host.clone(), c.name.clone()));
            }
        }
    }
    let stat_futs = targets.iter().map(|(host_addr, name)| {
        let host = config
            .hosts
            .iter()
            .find(|h| h.address == *host_addr)
            .map(yoink::docker_ops::Host::from);
        let ops = &ops;
        async move {
            let h = host?;
            ops.container_stats(&h, name).await.ok()
        }
    });
    let stats: Vec<Option<yoink::docker_ops::ContainerStats>> =
        futures_util::future::join_all(stat_futs).await;

    let mut rows: Vec<TopRow> = targets
        .into_iter()
        .zip(stats)
        .filter_map(|((host_addr, name), s)| {
            let s = s?;
            let container_info = report
                .hosts
                .iter()
                .find(|h| h.host == host_addr)
                .and_then(|h| h.containers.iter().find(|c| c.name == name));
            Some(TopRow {
                cpu_pct: s.cpu_pct,
                host: host_addr,
                service: container_info
                    .and_then(|c| c.yoink_service.clone())
                    .unwrap_or_else(|| "-".into()),
                container: name,
                mem_used: s.mem_used,
                mem_limit: s.mem_limit,
                created: container_info.and_then(|c| c.created_unix),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let trimmed: Vec<TopRow> = rows.into_iter().take(limit).collect();

    match format {
        TableFormat::Json => {
            let out: Vec<serde_json::Value> = trimmed
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "host": r.host,
                        "service": r.service,
                        "container": r.container,
                        "cpu_pct": r.cpu_pct,
                        "mem_used_bytes": r.mem_used,
                        "mem_limit_bytes": r.mem_limit,
                        "created_unix": r.created,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        TableFormat::Text => {
            println!(
                "{:<22}  {:<14}  {:<28}  {:>6}  {:>20}  created",
                "host", "service", "container", "cpu%", "mem"
            );
            for r in trimmed {
                let mem_str = match r.mem_limit {
                    Some(limit) if limit > 0 => {
                        format!("{} / {}", format_bytes(r.mem_used), format_bytes(limit))
                    }
                    _ => format_bytes(r.mem_used),
                };
                let created_str = format_relative_time(r.created);
                let cpu = r.cpu_pct;
                let host = r.host;
                let service = r.service;
                let container = r.container;
                println!(
                    "{host:<22}  {service:<14}  {container:<28}  {cpu:>5.1}%  {mem_str:>20}  {created_str}"
                );
            }
        }
    }
    Ok(())
}

async fn cmd_networks(config: &Config, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let probes = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|h| {
            let host = Host::from(h);
            let ops = &ops;
            async move {
                let nets = ops.list_networks(&host).await?;
                anyhow::Ok(nets)
            }
        });
    let mut all = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        all.extend(r?);
    }
    all.sort_by(|a, b| (a.host.as_str(), a.name.as_str()).cmp(&(b.host.as_str(), b.name.as_str())));
    println!(
        "{:<22}  {:<28}  {:<8}  {:<8}  {:>4}  internal",
        "host", "name", "driver", "scope", "ctrs"
    );
    for n in all {
        let internal = if n.internal { "yes" } else { "no" };
        let host = n.host;
        let name = n.name;
        let driver = n.driver;
        let scope = n.scope;
        let count = n.container_count;
        println!("{host:<22}  {name:<28}  {driver:<8}  {scope:<8}  {count:>4}  {internal}");
    }
    Ok(())
}

async fn cmd_volumes(config: &Config, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let probes = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|h| {
            let host = Host::from(h);
            let ops = &ops;
            async move {
                let vols = ops.list_volumes(&host).await?;
                anyhow::Ok(vols)
            }
        });
    let mut all = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        all.extend(r?);
    }
    all.sort_by(|a, b| (a.host.as_str(), a.name.as_str()).cmp(&(b.host.as_str(), b.name.as_str())));
    println!(
        "{:<22}  {:<40}  {:<10}  mountpoint",
        "host", "name", "driver"
    );
    for v in all {
        let host = v.host;
        let name = v.name;
        let driver = v.driver;
        let mp = v.mountpoint;
        println!("{host:<22}  {name:<40}  {driver:<10}  {mp}");
    }
    Ok(())
}

/// Substrings in env-var keys that mark the value as secret-ish —
/// those values render as `"<redacted>"` in the dump. Same heuristic
/// the TUI's container-detail pane uses; mirrored here so the dump
/// stays paste-safe.
const SECRET_KEY_HINTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASS",
    "KEY",
    "DSN",
    "CREDENTIAL",
    "SIGNING",
    "JWT",
    "COOKIE",
    "WEBHOOK",
    "BEARER",
    "PRIVATE",
    "SALT",
];

fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_HINTS.iter().any(|h| upper.contains(h))
}

/// Redact userinfo (`user:password@`) from URL-shaped values so a
/// credential-bearing URL doesn't leak the password when the env var
/// name itself doesn't trip `is_secret_key`.
fn redact_value(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let after = &value[scheme_end + 3..];
    let Some(at) = after.find('@') else {
        return value.to_string();
    };
    let host_part = &after[at..];
    let userinfo = &after[..at];
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    format!(
        "{}://{}:<redacted>{}",
        &value[..scheme_end],
        user,
        host_part
    )
}

#[allow(clippy::too_many_lines)]
async fn cmd_dump(config: &Config, log_tail: u32) -> Result<()> {
    use serde_json::json;
    use yoink::deploy;
    use yoink::docker;
    use yoink::lock::LOCK_NAME;

    // Best-effort secrets load — drift hashes are accurate when it
    // succeeds, marked "?" otherwise. Failure is logged via tracing
    // (silenced inside dump output). Loaded first so build_real_ops
    // can reuse it for any per-host ssh_key_secret resolution.
    let secrets = yoink::secrets::load_bundle(config).await.ok().flatten();
    let ops = std::sync::Arc::new(build_real_ops(config, secrets.as_ref()).await?)
        as std::sync::Arc<dyn DockerOps>;

    let mut hosts_json = Vec::new();
    let mut issues: Vec<String> = Vec::new();

    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        let address = host.address.clone();
        let mut host_obj = json!({
            "address": address,
            "user": host.user,
            "is_local": host.is_local(),
        });

        match ops.version(&host).await {
            Ok(v) => {
                host_obj["docker"] = json!({
                    "server_version": v.server_version,
                    "api_version": v.api_version,
                    "os": v.os,
                    "arch": v.arch,
                });
                host_obj["reachable"] = json!(true);
            }
            Err(e) => {
                host_obj["reachable"] = json!(false);
                host_obj["unreachable_error"] = json!(format!("{e:#}"));
                issues.push(format!("host {address} unreachable: {e}"));
                hosts_json.push(host_obj);
                continue;
            }
        }

        host_obj["host_info"] = match ops.host_info(&host).await {
            Ok(i) => json!({
                "n_cpu": i.n_cpu,
                "mem_total": i.mem_total,
                "containers": i.containers,
                "containers_running": i.containers_running,
                "images": i.images,
                "kernel": i.kernel,
                "operating_system": i.operating_system,
            }),
            Err(_) => serde_json::Value::Null,
        };
        host_obj["networks"] =
            serde_json::to_value(ops.list_networks(&host).await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null);
        host_obj["volumes"] =
            serde_json::to_value(ops.list_volumes(&host).await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null);

        // Lock state — find the sentinel by name.
        let lock_state = ops
            .list_running_containers(&host)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.name == LOCK_NAME))
            .map(|c| {
                json!({
                    "held": true,
                    "acquired_unix": c.created_unix,
                })
            })
            .unwrap_or(json!({ "held": false }));
        host_obj["deploy_lock"] = lock_state;

        // All yoink-managed containers (running + exited).
        let containers = ops
            .list_containers_by_label(&host, "yoink.managed=true")
            .await
            .unwrap_or_default();
        let mut container_objs = Vec::new();
        for c in containers {
            // Per-container stats (skip if not running).
            let stats = if c.is_running() {
                ops.container_stats(&host, &c.name).await.ok().map(|s| {
                    json!({
                        "cpu_pct": s.cpu_pct,
                        "mem_used": s.mem_used,
                        "mem_limit": s.mem_limit,
                    })
                })
            } else {
                None
            };
            let inspect = ops.inspect_container(&host, &c.name).await.ok();
            let log_tail_lines = ops
                .fetch_recent_logs(&host, &c.name, log_tail)
                .await
                .ok()
                .map(|ls| {
                    ls.into_iter()
                        .map(|l| l.trim_end_matches('\n').to_string())
                        .collect::<Vec<_>>()
                });

            // Drift hash for yoink-managed containers we have a config
            // for. Tag fallback: config's tag if set, else the
            // container's own yoink_version.
            let drift = c.yoink_service.as_deref().and_then(|name| {
                let svc = config.services.iter().find(|s| s.name == name)?;
                let tag = svc.tag.clone().or_else(|| c.yoink_version.clone())?;
                let desired =
                    deploy::build_desired_spec(config, svc, &tag, secrets.as_ref()).ok()?;
                let desired_hash = docker::compute_spec_hash(&desired);
                let running_hash = c.yoink_spec_hash.clone()?;
                Some(json!({
                    "status": if desired_hash == running_hash { "sync" } else { "drift" },
                    "desired_spec_hash": desired_hash,
                    "running_spec_hash": running_hash,
                    "tag_used_for_desired": tag,
                }))
            });
            if let Some(d) = &drift
                && d["status"] == "drift"
            {
                issues.push(format!(
                    "container {} ({}) is drifted from current config",
                    c.name,
                    c.yoink_service.as_deref().unwrap_or("?")
                ));
            }
            if !c.is_running() {
                issues.push(format!(
                    "container {} state={} (status: {})",
                    c.name, c.state, c.status_text
                ));
            }

            // Inspect with env redacted.
            let inspect_json = inspect.map(|i| {
                let env: Vec<serde_json::Value> = i
                    .env
                    .iter()
                    .map(|kv| {
                        let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
                        if is_secret_key(k) {
                            json!({"key": k, "value": "<redacted>"})
                        } else {
                            json!({"key": k, "value": redact_value(v)})
                        }
                    })
                    .collect();
                json!({
                    "image": i.image,
                    "image_id": i.image_id,
                    "command": i.command,
                    "working_dir": i.working_dir,
                    "state": i.state,
                    "status": i.status,
                    "started_at": i.started_at,
                    "finished_at": i.finished_at,
                    "exit_code": i.exit_code,
                    "restart_count": i.restart_count,
                    "restart_policy": i.restart_policy,
                    "pid": i.pid,
                    "ports": i.ports,
                    "mounts": i.mounts,
                    "networks": i.networks,
                    "labels": i.labels,
                    "env": env,
                })
            });

            container_objs.push(json!({
                "name": c.name,
                "image": c.image,
                "state": c.state,
                "status_text": c.status_text,
                "created_unix": c.created_unix,
                "yoink_service": c.yoink_service,
                "yoink_version": c.yoink_version,
                "yoink_spec_hash": c.yoink_spec_hash,
                "yoink_deployed_by": c.yoink_deployed_by,
                "yoink_deployed_at": c.yoink_deployed_at,
                "networks": c.networks,
                "other_labels": c.other_labels,
                "stats": stats,
                "inspect": inspect_json,
                "log_tail": log_tail_lines,
                "drift": drift,
            }));
        }
        host_obj["containers"] = json!(container_objs);

        hosts_json.push(host_obj);
    }

    let dump = json!({
        "yoink": {
            "version": env!("CARGO_PKG_VERSION"),
            "generated_at_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            "secrets_bundle_loaded": secrets.is_some(),
        },
        "config": {
            "deploy": {
                "networks": config.deploy.networks,
            },
            "hosts": config.hosts.iter().map(|h| {
                json!({"address": h.address, "user": h.user})
            }).collect::<Vec<_>>(),
            "services": config.services.iter().map(|s| {
                let networks = s.networks.clone().unwrap_or_else(|| config.deploy.networks.clone());
                json!({
                    "name": s.name,
                    "image": s.image,
                    "tag_pinned": s.tag,
                    "networks": networks,
                    "env_keys": s.env.keys().collect::<Vec<_>>(),
                    "secret_keys": s.secrets,
                    "env_from_secret_keys": s.env_from_secrets.keys().collect::<Vec<_>>(),
                    "host_filter": s.hosts,
                    "replicas": s.run.replicas,
                    "port": s.run.port,
                    "healthcheck_path": s.run.healthcheck_path,
                    "publish": s.run.publish,
                    "binds": s.run.binds,
                    "volumes": s.run.volumes,
                    "files": s.run.files,
                    "labels": s.labels,
                })
            }).collect::<Vec<_>>(),
            "registry_server": config.registry.as_ref().map(|r| r.server.clone()),
            "secrets_provider": config.secrets.as_ref().map(|s| match s {
                yoink::config::SecretsConfig::Age { .. } => "age",
                yoink::config::SecretsConfig::Command { .. } => "command",
            }),
        },
        "hosts": hosts_json,
        "issues": issues,
    });

    println!("{}", serde_json::to_string_pretty(&dump)?);
    Ok(())
}

async fn cmd_validate(config: &Config, check_hosts: bool) -> Result<()> {
    // `Config::load_from_path` (called from `run`) already ran the
    // structural checks — duplicate names/addresses, missing fields,
    // unknown YAML keys. If we got this far the config is well-formed.
    println!(
        "✓ config OK — {} services across {} hosts",
        config.services.len(),
        config.hosts.len()
    );
    // Caddy schema validation: render the proxy config and run it
    // through `caddy validate` in an ephemeral container. Catches
    // schema errors (invalid `caddy_extra_json`, bad TLS config,
    // unknown handler module) BEFORE deploy time. Best-effort —
    // skips silently if docker isn't on the operator's machine.
    if yoink::proxy::proxy_enabled(config) {
        validate_proxy_render(config).await?;
    }
    if check_hosts {
        cmd_preflight(config, None).await?;
    }
    Ok(())
}

async fn cmd_doctor(config: &Config, json: bool) -> Result<()> {
    use yoink::doctor::{Severity, run_doctor, tally};

    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(build_real_ops(config, None).await?);
    let findings = run_doctor(config, ops).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        for f in &findings {
            let icon = match f.severity {
                Severity::Pass => "✓",
                Severity::Warn => "!",
                Severity::Error => "✗",
            };
            eprintln!("{icon} [{}] {}", f.category, f.title);
            if let Some(d) = &f.detail {
                for line in d.lines() {
                    eprintln!("    {line}");
                }
            }
            if let Some(fix) = &f.fix {
                eprintln!("    fix: {fix}");
            }
        }
        eprintln!();
        let (pass, warn, err) = tally(&findings);
        eprintln!("summary: {pass} pass, {warn} warn, {err} error");
    }

    let (_, _, errors) = tally(&findings);
    if errors > 0 {
        anyhow::bail!("doctor found blocking issues");
    }
    Ok(())
}

async fn validate_proxy_render(config: &Config) -> Result<()> {
    let bundle = load_secrets_bundle(config).await?;
    let mut config = config.clone();
    yoink::proxy::caddy::expand_caddyfile_snippets(&mut config)
        .await
        .context("expand caddy_extra_caddyfile snippets")?;
    let json = yoink::proxy::caddy::render(&config, |_| Vec::new(), bundle.as_ref())
        .context("render Caddy config")?;
    let body = serde_json::to_string(&json).context("serialize rendered config")?;

    // Probe for a local docker daemon. Skip gracefully if absent.
    let docker_avail = tokio::process::Command::new("docker")
        .arg("version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !docker_avail {
        eprintln!(
            "  (skipping Caddy schema check — docker not available locally; \
             pass --check-hosts or run on a host with docker installed)"
        );
        return Ok(());
    }

    // With `proxy.xcaddy:` the proxy image is built per-host as
    // `yoink-caddy:<hash>` and doesn't exist on the operator's machine,
    // so spawning `docker run yoink-caddy:<hash>` here would 404. Skip
    // — the rendered config still ran through `proxy::caddy::render`
    // above (catches schema errors), and Caddy refuses bad configs at
    // `/load` time on the host anyway.
    if config.proxy.as_ref().is_some_and(|p| p.xcaddy.is_some()) {
        eprintln!(
            "  (skipping Caddy schema check — proxy.xcaddy is set; the proxy image only \
             exists on hosts. Caddy will reject any bad config at /load time.)"
        );
        return Ok(());
    }

    let image = config.proxy.as_ref().map_or_else(
        || yoink::config::CADDY_DEFAULT_IMAGE.to_string(),
        yoink::config::ProxyConfig::resolved_image,
    );
    let mut child = tokio::process::Command::new("docker")
        // JSON is Caddy's native config format — no `--adapter` flag.
        // (`--adapter caddyfile` would convert from Caddyfile syntax;
        // we feed JSON directly.)
        .args([
            "run",
            "--rm",
            "-i",
            &image,
            "caddy",
            "validate",
            "--config",
            "/dev/stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn `docker run caddy validate`")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(body.as_bytes())
            .await
            .context("write rendered config to caddy validate stdin")?;
        // Drop closes stdin → caddy reads EOF → validation runs.
    }
    let output = child
        .wait_with_output()
        .await
        .context("wait on `docker run caddy validate`")?;
    if output.status.success() {
        println!("✓ Caddy config schema valid");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let extras: Vec<&str> = config
            .services
            .iter()
            .filter(|s| s.caddy_extra_json.is_some())
            .map(|s| s.name.as_str())
            .collect();
        let hint = if extras.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nhint: services with caddy_extra_json: {} — \
                 run `yoink proxy-render` to inspect the rendered config",
                extras.join(", "),
            )
        };
        anyhow::bail!("Caddy rejected the rendered config:\n{stderr}{hint}");
    }
}

fn cmd_proxy_dockerfile(config: &Config) -> Result<()> {
    let xcaddy = config
        .proxy
        .as_ref()
        .and_then(|p| p.xcaddy.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "proxy.xcaddy is not set — there's no Dockerfile to render. Add a \
                 `proxy.xcaddy.plugins:` block first."
            )
        })?;
    let dockerfile = yoink::proxy::xcaddy::render_dockerfile(xcaddy);
    print!("{dockerfile}");
    Ok(())
}

async fn cmd_proxy_render(config: &Config) -> Result<()> {
    if !yoink::proxy::proxy_enabled(config) {
        anyhow::bail!(
            "proxy is not enabled — no service has `domain:` and `proxy.enabled` is unset"
        );
    }
    // Loads the secrets bundle when configured so `tls_*_secret` and
    // `client_auth.trust_pool_secret` references resolve. Empty
    // upstream lookup → render uses service-name fallbacks.
    let bundle = load_secrets_bundle(config).await?;
    let mut config = config.clone();
    yoink::proxy::caddy::expand_caddyfile_snippets(&mut config)
        .await
        .context("expand caddy_extra_caddyfile snippets")?;
    let json = yoink::proxy::caddy::render(&config, |_| Vec::new(), bundle.as_ref())
        .context("render Caddy config")?;
    let pretty = serde_json::to_string_pretty(&json).context("serialize rendered config")?;
    println!("{pretty}");
    Ok(())
}

async fn cmd_lock(config: &Config, action: LockAction) -> Result<()> {
    use yoink::lock::LOCK_NAME;
    let ops = build_real_ops(config, None).await?;
    match action {
        LockAction::Status => {
            // Fan out across hosts — one slow daemon shouldn't make
            // the others wait. Per-host errors surface as Err inline.
            let probes = config.hosts.iter().map(|host_cfg| {
                let host = Host::from(host_cfg);
                let ops = &ops;
                async move {
                    let containers = ops.list_running_containers(&host).await?;
                    let held = containers
                        .iter()
                        .find(|c| c.name == LOCK_NAME)
                        .map(|c| output::format_relative_time(c.created_unix));
                    anyhow::Ok((host.address, held))
                }
            });
            let results = futures_util::future::join_all(probes).await;
            for r in results {
                match r {
                    Ok((addr, Some(age))) => println!("{addr}: HELD (acquired {age})"),
                    Ok((addr, None)) => println!("{addr}: free"),
                    Err(e) => eprintln!("{e:#}"),
                }
            }
        }
        LockAction::Release { host, yes } => {
            if !yes {
                eprintln!(
                    "force-releasing the deploy lock while another operator is deploying \
                     will corrupt that deploy. pass --yes to confirm."
                );
                anyhow::bail!("aborted");
            }
            for host_cfg in &config.hosts {
                if let Some(filter) = &host
                    && filter != &host_cfg.address
                {
                    continue;
                }
                let h = Host::from(host_cfg);
                match ops.force_remove_container(&h, LOCK_NAME).await {
                    Ok(()) => eprintln!("{}: released", h.address),
                    Err(e) => eprintln!("{}: {e:#}", h.address),
                }
            }
        }
    }
    Ok(())
}

async fn cmd_diff(config: &Config, service: &str, tag_override: Option<&str>) -> Result<()> {
    let svc_cfg = config
        .services
        .iter()
        .find(|s| s.name == service)
        .with_context(|| format!("service {service:?} not in config"))?;
    let target_tag = tag_override
        .map(str::to_string)
        .or_else(|| svc_cfg.tag.clone())
        .unwrap_or_else(|| "<git>".into());

    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect_for_service(&ops, config, service)
        .await
        .context("collect status")?;
    let mut printed = false;
    for host in &report.hosts {
        for c in &host.containers {
            if !c.is_running() {
                continue;
            }
            let current = c.yoink_version.as_deref().unwrap_or("?");
            let same = current == target_tag;
            let arrow = if same { "==" } else { "→" };
            println!(
                "{}/{}: {} {} {}{}",
                host.host,
                c.name,
                current,
                arrow,
                target_tag,
                if same { " (no change)" } else { "" }
            );
            printed = true;
        }
    }
    if !printed {
        println!(
            "no running container for {service:?} — `up` would create the first one with tag {target_tag}"
        );
    }
    println!(
        "\nspec: {} (env, ports, mounts compared at deploy time via spec hash)",
        yoink::docker::image_reference(&svc_cfg.image, &target_tag),
    );
    Ok(())
}

fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut io::stdout());
}

/// Print one value per line. Output is consumed by the bash/zsh
/// snippet in the CLI reference; stays terse on purpose. `Configs`
/// is handled in `run_bootstrap` because completing config paths is
/// the one case that *must* work without a config already loaded.
fn cmd_complete(config: &Config, what: CompleteKind) {
    match what {
        CompleteKind::Services => {
            for svc in &config.services {
                println!("{}", svc.name);
            }
        }
        CompleteKind::Hosts => {
            for host in &config.hosts {
                println!("{}", host.address);
            }
        }
        CompleteKind::Configs => {
            // Bootstrap should have caught this; render anyway in
            // case someone calls through `run()` directly.
            yoink::completion::print_yoink_configs();
        }
    }
}

/// Subcommands that don't need a `yoink.yaml`. Returns `Some(result)`
/// to short-circuit `run`'s config-load step, or `None` to fall
/// through.
fn run_bootstrap(command: &Command) -> Option<Result<()>> {
    match command {
        Command::Completions { shell } => {
            cmd_completions(*shell);
            Some(Ok(()))
        }
        Command::Complete {
            what: CompleteKind::Configs,
        } => {
            // Configs completion *cannot* depend on a config already
            // existing — the whole point is to find one to use. Run
            // before the load step in `run()`.
            yoink::completion::print_yoink_configs();
            Some(Ok(()))
        }
        Command::Secrets {
            action:
                SecretsAction::Key {
                    action: KeyAction::Generate { out, force, print },
                },
        } => Some(cmd_secrets_key_generate(out.clone(), *force, *print)),
        Command::Init {
            host,
            force,
            interactive,
            service,
            port,
            no_port,
            image,
            no_secrets,
        } => Some(yoink::init::cmd_init(yoink::init::InitOpts {
            host: host.clone(),
            force: *force,
            interactive: *interactive,
            service: service.clone(),
            port: *port,
            no_port: *no_port,
            image: image.clone(),
            no_secrets: *no_secrets,
        })),
        _ => None,
    }
}

fn cmd_secrets(config: &Config, action: SecretsAction) -> Result<()> {
    match action {
        SecretsAction::Key { action } => match action {
            KeyAction::Generate { out, force, print } => {
                cmd_secrets_key_generate(out, force, print)
            }
            KeyAction::Public => cmd_secrets_key_public(config),
        },
        SecretsAction::Edit => cmd_secrets_edit(config),
        SecretsAction::Show { reveal } => cmd_secrets_show(config, reveal),
        SecretsAction::Seal {
            r#in,
            as_pairs,
            out,
        } => cmd_secrets_seal(config, r#in.as_deref(), &as_pairs, out),
        SecretsAction::Rotate => cmd_secrets_rotate(config),
        SecretsAction::SshKey { action } => match action {
            SshKeyAction::Generate { seal_as, comment } => {
                cmd_secrets_ssh_key_generate(config, &seal_as, comment.as_deref())
            }
            SshKeyAction::Public { name } => cmd_secrets_ssh_key_public(config, &name),
        },
    }
}

fn cmd_secrets_ssh_key_generate(
    config: &Config,
    seal_as: &str,
    comment: Option<&str>,
) -> Result<()> {
    use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};
    use yoink::sealed;

    if seal_as.is_empty()
        || seal_as
            .bytes()
            .next()
            .is_none_or(|b| !(b.is_ascii_alphabetic() || b == b'_'))
        || !seal_as
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(anyhow::anyhow!(
            "--seal-as {seal_as:?} is not a valid secret name (must match [A-Za-z_][A-Za-z0-9_]*)"
        ));
    }

    let (file_override, recipients) = expect_age_block(config)?;
    let target = sealed::resolve_sealed_path(config, file_override.as_deref())?;

    // ed25519 keypair, in memory only.
    let mut rng = OsRng;
    let key =
        PrivateKey::random(&mut rng, Algorithm::Ed25519).context("generate ed25519 keypair")?;
    let key = if let Some(c) = comment {
        let mut k = key;
        k.set_comment(c);
        k
    } else {
        key
    };
    let priv_pem = key
        .to_openssh(LineEnding::LF)
        .context("encode private key as OpenSSH PEM")?;
    let pub_openssh = key
        .public_key()
        .to_openssh()
        .context("encode public key as OpenSSH")?;

    // Merge into the existing bundle (if any) so we don't clobber
    // unrelated keys. Same shape as `yoink secrets edit` on save.
    let mut bundle = if target.exists() {
        let identity = sealed::load_identity(recipients)?;
        let ciphertext =
            std::fs::read(&target).with_context(|| format!("read {}", target.display()))?;
        let plaintext = sealed::unseal(&ciphertext, &identity)?;
        sealed::parse_dotenv(&plaintext)?
    } else {
        std::collections::BTreeMap::new()
    };
    if bundle.contains_key(seal_as) {
        return Err(anyhow::anyhow!(
            "{seal_as:?} already exists in the sealed bundle. Pick a different --seal-as name, \
             or run `yoink secrets edit` to remove the existing entry first."
        ));
    }
    bundle.insert(seal_as.to_string(), (*priv_pem).clone());

    let canonical = sealed::render_dotenv(&bundle);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    sealed::write_atomically_secret(&target, &sealed_bytes)?;

    eprintln!(
        "✓ sealed new ed25519 SSH private key as {seal_as:?} into {}",
        target.display()
    );
    // stdout: just the public key, one line, ready to pipe.
    println!("{pub_openssh}");
    Ok(())
}

fn cmd_secrets_ssh_key_public(config: &Config, name: &str) -> Result<()> {
    use ssh_key::PrivateKey;
    use yoink::sealed;

    let (file_override, recipients) = expect_age_block(config)?;
    let target = sealed::resolve_sealed_path(config, file_override.as_deref())?;
    if !target.exists() {
        return Err(anyhow::anyhow!(
            "no sealed bundle at {}; nothing to derive {name:?} from",
            target.display()
        ));
    }
    let identity = sealed::load_identity(recipients)?;
    let ciphertext =
        std::fs::read(&target).with_context(|| format!("read {}", target.display()))?;
    let plaintext = sealed::unseal(&ciphertext, &identity)?;
    let bundle = sealed::parse_dotenv(&plaintext)?;
    let pem = bundle.get(name).ok_or_else(|| {
        anyhow::anyhow!(
            "{name:?} not found in the sealed bundle at {}",
            target.display()
        )
    })?;
    let key = PrivateKey::from_openssh(pem.as_bytes())
        .with_context(|| format!("{name:?} is not a valid OpenSSH-format private key"))?;
    let pub_openssh = key
        .public_key()
        .to_openssh()
        .context("encode public key as OpenSSH")?;
    // stdout: just the public key, one line, ready to pipe.
    println!("{pub_openssh}");
    Ok(())
}

fn cmd_secrets_key_public(config: &Config) -> Result<()> {
    use yoink::config::SecretsConfig;
    use yoink::sealed;
    // Pull recipients from the config so the keys-dir scan can pick
    // the matching identity. If the operator has many keys in
    // ~/.config/yoink/keys/, this answers "the one for *this*
    // project," not whichever happened to load first.
    let recipients: &[String] = match &config.secrets {
        Some(SecretsConfig::Age { recipients, .. }) => recipients,
        _ => &[],
    };
    let identity = sealed::load_identity(recipients)
        .with_context(|| "no age identity found — set YOINK_AGE_KEY / YOINK_AGE_KEY_FILE, or place a key in ~/.config/yoink/keys/")?;
    println!("{}", identity.to_public());
    Ok(())
}

fn cmd_secrets_key_generate(out: Option<PathBuf>, force: bool, print: bool) -> Result<()> {
    use yoink::sealed;
    let (secret, public) = sealed::keygen();
    let recipient_block =
        format!("  secrets:\n    provider: age\n    recipients:\n      - {public}");

    if print {
        // Explicit stdout mode — operator pipes / pastes themselves.
        // Discipline: only the *secret itself* goes to stdout, so a
        // pipe like `... --print | gh secret set YOINK_AGE_KEY`
        // captures exactly the key bytes. Everything else (header,
        // recipient, follow-up instructions) goes to stderr where the
        // operator reads it without contaminating the pipe.
        eprintln!("New age identity. Save the secret somewhere — yoink won't.");
        eprintln!();
        eprintln!("Secret (private — never commit) — piped to stdout:");
        println!("{secret}");
        eprintln!();
        eprintln!("Public recipient (add to yoink.yaml):");
        eprintln!();
        eprintln!("{recipient_block}");
        eprintln!();
        eprintln!("Suggested next steps:");
        eprintln!(
            "  • Pipe into a CI secret: `yoink secrets key generate --print | gh secret set YOINK_AGE_KEY`"
        );
        eprintln!("  • Or pipe into a password manager (`op item create … password=-`).");
        eprintln!("  • Clear your terminal scrollback when done.");
        return Ok(());
    }

    // Default: write to ~/.config/yoink/keys/<public>.key. Filename =
    // public recipient lets `load_identity` find it cheaply (and lets
    // multiple projects coexist — one identity per project, no
    // collisions). `--out` overrides the destination.
    let path = match out {
        Some(p) => p,
        None => {
            let dir = sealed::keys_dir()?;
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            sealed::keys_dir_path_for(&public)?
        }
    };
    if path.exists() && !force {
        return Err(anyhow::anyhow!(
            "{} already exists — pass --force to overwrite",
            path.display()
        ));
    }
    let body = format!(
        "# created: {}\n# public key: {public}\n{secret}\n",
        chrono_like_now(),
    );
    sealed::write_atomically_secret(&path, body.as_bytes())?;
    eprintln!("wrote identity to {} (mode 0600)", path.display());
    eprintln!("public recipient: {public}");
    eprintln!();
    eprintln!("Add this to yoink.yaml:");
    eprintln!();
    eprintln!("{recipient_block}");
    if path.starts_with(sealed::keys_dir().unwrap_or_default()) {
        // In the default keys dir — yoink will discover it
        // automatically next time, no env var needed.
        eprintln!();
        eprintln!("yoink will discover this key automatically when sealing/unsealing.");
    } else {
        // Project-local or operator-chosen path — they own the
        // gitignore / env-var dance.
        eprintln!();
        eprintln!("Make sure {} is gitignored, then:", path.display());
        eprintln!("  export YOINK_AGE_KEY_FILE={}", path.display());
    }
    Ok(())
}

fn cmd_secrets_edit(config: &Config) -> Result<()> {
    use yoink::sealed;
    let (file_override, recipients) = expect_age_block(config)?;
    let path = sealed::resolve_sealed_path(config, file_override.as_deref())?;
    let plaintext = if path.exists() {
        let bytes =
            std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
        let identity = sealed::load_identity(recipients)?;
        sealed::unseal(&bytes, &identity)?
    } else {
        String::from("# yoink secrets — KEY=value, one per line\n")
    };
    let edited = open_in_editor(&plaintext)?;
    let parsed = sealed::parse_dotenv(&edited)?;
    let canonical = sealed::render_dotenv(&parsed);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    sealed::write_atomically_secret(&path, &sealed_bytes)?;
    eprintln!("sealed {} key(s) to {}", parsed.len(), path.display());
    Ok(())
}

fn cmd_secrets_show(config: &Config, reveal: bool) -> Result<()> {
    use yoink::config::SecretsConfig;
    use yoink::sealed;
    if reveal
        && let Some(ci_var) = detected_ci_env()
        && !is_truthy_env("YOINK_ALLOW_REVEAL_IN_CI")
    {
        // CI runners log stdout into build artifacts that get
        // shared / archived / scraped — `--reveal` printing real
        // values there is almost always a mistake. Force operators
        // to override consciously when they really mean it.
        return Err(anyhow::anyhow!(
            "refusing to print real secret values: detected CI environment (${ci_var} is set). \
             If this is intentional (you're capturing the bundle into a managed secret store, \
             not into build logs), set YOINK_ALLOW_REVEAL_IN_CI=1"
        ));
    }
    let SecretsConfig::Age { file, recipients } = expect_secrets_provider_age(config)? else {
        unreachable!()
    };
    let path = sealed::resolve_sealed_path(config, file.as_deref())?;
    let bytes =
        std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
    let identity = sealed::load_identity(recipients)?;
    let plaintext = sealed::unseal(&bytes, &identity)?;
    let parsed = sealed::parse_dotenv(&plaintext)?;
    for (k, v) in &parsed {
        if reveal {
            println!("{k}={v}");
        } else {
            println!("{k}={}", mask_value(v));
        }
    }
    Ok(())
}

fn cmd_secrets_seal(
    config: &Config,
    input: Option<&Path>,
    as_pairs: &[String],
    out: Option<PathBuf>,
) -> Result<()> {
    use yoink::sealed;
    let (file_override, recipients) = expect_age_block(config)?;
    // Cap input at 10 MiB regardless of source — protects against
    // a wrong-file paste (a 5GB image) just as much as an unbounded
    // stdin stream. Real secrets bundles are kilobytes.
    const SEAL_INPUT_CAP: u64 = 10 * 1024 * 1024;
    let parsed = if !as_pairs.is_empty() {
        parse_as_pairs(as_pairs, SEAL_INPUT_CAP)?
    } else {
        let plaintext = match input {
            Some(p) if p.as_os_str() != "-" => {
                use std::io::Read;
                let f = std::fs::File::open(p)
                    .with_context(|| format!("open input {}", p.display()))?;
                let mut buf = String::new();
                f.take(SEAL_INPUT_CAP + 1)
                    .read_to_string(&mut buf)
                    .with_context(|| format!("read input {}", p.display()))?;
                if buf.len() as u64 > SEAL_INPUT_CAP {
                    return Err(anyhow::anyhow!(
                        "input file {} is larger than {SEAL_INPUT_CAP} bytes — refusing to seal an oversized bundle",
                        p.display()
                    ));
                }
                buf
            }
            _ => {
                use std::io::Read;
                let mut buf = String::new();
                io::stdin()
                    .take(SEAL_INPUT_CAP + 1)
                    .read_to_string(&mut buf)?;
                if buf.len() as u64 > SEAL_INPUT_CAP {
                    return Err(anyhow::anyhow!(
                        "stdin produced more than {SEAL_INPUT_CAP} bytes — refusing to seal an oversized bundle"
                    ));
                }
                buf
            }
        };
        sealed::parse_dotenv(&plaintext)?
    };
    let canonical = sealed::render_dotenv(&parsed);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    let target = match out {
        Some(p) => p,
        None => sealed::resolve_sealed_path(config, file_override.as_deref())?,
    };
    sealed::write_atomically_secret(&target, &sealed_bytes)?;
    println!("sealed {} key(s) to {}", parsed.len(), target.display());
    Ok(())
}

/// Parse `--as KEY=value` and `--as KEY=@PATH` pairs into a sealable
/// map. Each entry is verified at parse time so a typo at the front of
/// the list doesn't get a partial seal — either every pair is valid or
/// the whole call errors.
fn parse_as_pairs(
    as_pairs: &[String],
    cap_bytes: u64,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    for raw in as_pairs {
        let (key, rhs) = raw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "--as expects `KEY=value` or `KEY=@PATH`, got {raw:?} (no `=` separator)"
            )
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(anyhow::anyhow!(
                "--as got an empty KEY in {raw:?}; expected `KEY=value` or `KEY=@PATH`"
            ));
        }
        let value = if let Some(path_str) = rhs.strip_prefix('@') {
            use std::io::Read;
            let path = Path::new(path_str);
            let f = std::fs::File::open(path)
                .with_context(|| format!("--as {key}=@{path_str}: open"))?;
            let mut buf = String::new();
            f.take(cap_bytes + 1)
                .read_to_string(&mut buf)
                .with_context(|| format!("--as {key}=@{path_str}: read"))?;
            if buf.len() as u64 > cap_bytes {
                return Err(anyhow::anyhow!(
                    "--as {key}=@{path_str}: file is larger than {cap_bytes} bytes"
                ));
            }
            buf
        } else {
            rhs.to_string()
        };
        if out.insert(key.to_string(), value).is_some() {
            return Err(anyhow::anyhow!(
                "--as {key}=… given more than once; pass each KEY at most once"
            ));
        }
    }
    Ok(out)
}

fn cmd_secrets_rotate(config: &Config) -> Result<()> {
    use yoink::sealed;

    let (file_override, current_recipients) = expect_age_block(config)?;
    let path = sealed::resolve_sealed_path(config, file_override.as_deref())?;
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "{} doesn't exist — nothing to rotate. `yoink secrets edit` to create it first",
            path.display()
        ));
    }

    // Decrypt with the current identity before generating the new key.
    let bytes =
        std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
    let identity = sealed::load_identity(current_recipients)?;
    let plaintext = sealed::unseal(&bytes, &identity)?;
    let parsed = sealed::parse_dotenv(&plaintext)?;
    let canonical = sealed::render_dotenv(&parsed);

    // Generate the new identity. The secret never lands on disk.
    let (new_secret, new_public) = sealed::keygen();

    // Re-seal under [current recipients ∪ new_public].
    let mut next: Vec<String> = current_recipients.clone();
    if !next.iter().any(|r| r == &new_public) {
        next.push(new_public.clone());
    }
    let resealed = sealed::seal(canonical.as_bytes(), &next)?;
    sealed::write_atomically_secret(&path, &resealed)?;

    eprintln!(
        "re-sealed {} key(s) to {} ({} recipients)",
        parsed.len(),
        path.display(),
        next.len()
    );
    eprintln!();
    eprintln!("New CI identity (paste into GitHub Actions secret YOINK_AGE_KEY):");
    eprintln!();
    // The secret itself is the only stdout output — designed to pipe
    // into a CI secret store (`… --print | gh secret set YOINK_AGE_KEY`).
    println!("{new_secret}");
    eprintln!();
    eprintln!("New public recipient (add to yoink.yaml under `secrets.recipients:`):");
    eprintln!();
    eprintln!("  - {new_public}");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add the new recipient to yoink.yaml so future edits include it.");
    eprintln!("  2. Update the YOINK_AGE_KEY GitHub secret to the value above.");
    eprintln!("  3. Once CI is happily decrypting with the new key, remove the OLD");
    eprintln!("     recipient from yoink.yaml and run `yoink secrets edit` (save");
    eprintln!("     without changes) to drop it from the sealed file.");
    Ok(())
}

fn expect_secrets_provider_age(config: &Config) -> Result<&yoink::config::SecretsConfig> {
    use yoink::config::SecretsConfig;
    match config.secrets.as_ref() {
        Some(s @ SecretsConfig::Age { .. }) => Ok(s),
        Some(SecretsConfig::Command { .. }) => Err(anyhow::anyhow!(
            "yoink.yaml configures `provider: command` — `yoink secrets` only manages age-sealed files. \
             To rotate values in your external secret store, use that store's CLI directly."
        )),
        None => Err(anyhow::anyhow!(
            "no `secrets:` block in yoink.yaml — add `secrets: {{ provider: age, recipients: [...] }}` first (see `yoink secrets key generate`)"
        )),
    }
}

fn expect_age_block(config: &Config) -> Result<(Option<String>, &Vec<String>)> {
    use yoink::config::SecretsConfig;
    let SecretsConfig::Age { file, recipients } = expect_secrets_provider_age(config)? else {
        unreachable!()
    };
    if recipients.is_empty() {
        return Err(anyhow::anyhow!(
            "no `secrets.recipients:` configured — add at least one age public key (`age1...`) to yoink.yaml"
        ));
    }
    Ok((file.clone(), recipients))
}

fn open_in_editor(initial: &str) -> Result<String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    // `$EDITOR` commonly carries flags (`code --wait`, `nvim --noplugin`,
    // `emacsclient -nw`). Treat the whole string as a shell-style cmd
    // by splitting on ASCII whitespace; the first token is the program
    // and the rest are forwarded as args before the scratch path.
    let mut tokens = editor.split_ascii_whitespace();
    let program = tokens
        .next()
        .ok_or_else(|| anyhow::anyhow!("EDITOR/VISUAL is empty"))?;
    let editor_args: Vec<&str> = tokens.collect();

    // tempfile::NamedTempFile gives us:
    //   - randomized filename (no PID-predictable path another user
    //     on the box could pre-create as a symlink)
    //   - RAII cleanup, so the plaintext is removed even if a panic
    //     unwinds past the explicit drop below
    //   - O_EXCL semantics under the hood
    let mut scratch = yoink::sealed::secret_tempfile_builder(Some(0o600))
        .suffix(".env")
        .prefix("yoink-secrets-")
        .tempfile()
        .context("create scratch file")?;
    {
        use std::io::Write as _;
        scratch
            .as_file_mut()
            .write_all(initial.as_bytes())
            .with_context(|| format!("write {}", scratch.path().display()))?;
    }

    let status = std::process::Command::new(program)
        .args(&editor_args)
        .arg(scratch.path())
        .status()
        .with_context(|| format!("launch editor {editor:?}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "editor {editor:?} exited with {status} — aborting"
        ));
    }

    let edited = std::fs::read_to_string(scratch.path())
        .with_context(|| format!("read edited file {}", scratch.path().display()))?;
    // Drop runs unlink(2); explicit `close()` would let us surface a
    // cleanup error but we'd rather not fail the seal on a tmpfs hiccup.
    drop(scratch);
    Ok(edited)
}

fn mask_value(s: &str) -> String {
    let visible: usize = if s.len() <= 4 { 0 } else { 2 };
    let prefix: String = s.chars().take(visible).collect();
    let masked = "•".repeat(s.chars().count().saturating_sub(visible).min(16));
    format!("{prefix}{masked}")
}

/// Returns the name of the first CI-environment env var that is set,
/// or None for an interactive shell. The CI=1 convention is set by
/// most providers but not all (some only set their own per-product
/// var); checking the union catches more cases.
fn detected_ci_env() -> Option<&'static str> {
    const CI_ENV_VARS: &[&str] = &[
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "CIRCLECI",
        "BUILDKITE",
        "TRAVIS",
        "TF_BUILD", // Azure Pipelines
        "TEAMCITY_VERSION",
        "BITBUCKET_BUILD_NUMBER",
        "DRONE",
        "JENKINS_URL",
    ];
    CI_ENV_VARS
        .iter()
        .copied()
        .find(|name| std::env::var_os(name).is_some())
}

/// Strict truthy parse for guard-override env vars. `"1"`, `"true"`,
/// `"yes"`, `"on"` (case-insensitive) flip; everything else — empty
/// string, `"0"`, `"false"`, `"no"`, `"off"`, unset — does not. Avoids
/// the trap where a misconfigured workflow sets the override to an
/// empty value (e.g. `FOO: ${{ secrets.MISSING }}`) and silently
/// bypasses the guard.
fn is_truthy_env(name: &str) -> bool {
    let Some(raw) = std::env::var_os(name) else {
        return false;
    };
    let s = raw.to_string_lossy();
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("unix={secs}")
}

#[cfg(test)]
mod tests {
    use super::parse_as_pairs;

    #[test]
    fn as_pair_literal_value() {
        let m = parse_as_pairs(&["FOO=bar".into()], 1024).unwrap();
        assert_eq!(m.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn as_pair_value_can_contain_equals_and_at() {
        let m = parse_as_pairs(&["URL=http://x.example.com/a=1".into()], 1024).unwrap();
        assert_eq!(m.get("URL"), Some(&"http://x.example.com/a=1".to_string()));
    }

    #[test]
    fn as_pair_at_path_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body");
        std::fs::write(&path, "multi\nline\nvalue").unwrap();
        let arg = format!("KEY=@{}", path.display());
        let m = parse_as_pairs(&[arg], 1024).unwrap();
        assert_eq!(m.get("KEY"), Some(&"multi\nline\nvalue".to_string()));
    }

    #[test]
    fn as_pair_missing_separator_errors() {
        let err = parse_as_pairs(&["NO_EQUALS".into()], 1024).unwrap_err();
        assert!(format!("{err:#}").contains("--as expects"));
    }

    #[test]
    fn as_pair_empty_key_errors() {
        let err = parse_as_pairs(&["=value".into()], 1024).unwrap_err();
        assert!(format!("{err:#}").contains("empty KEY"));
    }

    #[test]
    fn as_pair_duplicate_key_errors() {
        let err = parse_as_pairs(&["K=a".into(), "K=b".into()], 1024).unwrap_err();
        assert!(format!("{err:#}").contains("more than once"));
    }

    #[test]
    fn as_pair_at_path_respects_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, vec![b'x'; 100]).unwrap();
        let arg = format!("KEY=@{}", path.display());
        let err = parse_as_pairs(&[arg], 50).unwrap_err();
        assert!(format!("{err:#}").contains("larger than"));
    }
}
