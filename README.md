# yoink

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of hosts and want a deploy story that fits in their head.

```
yoink up                  # reconcile every service in dep order
yoink tui                 # interactive dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

## What it is

`yoink` drives Docker on remote hosts via the SSH transport (`ssh://user@host`). One config file describes your services, dependencies, networks, secrets, and healthchecks; `yoink up` reconciles the cluster to that spec via dep-ordered, healthcheck-gated rolling deploys. There is a `ratatui`-based TUI for the day-to-day operator experience: drift detection across hosts, live logs, shell into containers, one-key reconcile + prune.

It is not Kubernetes. It is not Nomad. It is not trying to be Kamal. It is the smallest tool that solves "deploy N services across N hosts without breaking traffic, with an operator UX that doesn't suck".

## Design choices

- **No agent on the hosts.** yoink is a single Rust binary that talks to remote Docker via the bollard `ssh://` transport. Nothing runs on the hosts you don't already run.
- **Config is the source of truth.** State of the world is queried from Docker (containers labeled `yoink.service=<name>` and `yoink.spec_hash=<hex>`); no local state files.
- **Spec hashing for drift detection.** Every service's effective spec (image, env, networks, mounts, options, file content hashes) hashes deterministically. Drift is a label compare, not guessing.
- **Dependency-ordered deploys.** Services declare `depends_on:` and `yoink up` runs them in a topological-sort wave (independent services in parallel). No more "I redeployed api before redis was up".
- **Healthcheck-gated swap.** New container starts; HTTP or TCP probe must pass before the old one is stopped. Configurable timeout, drain.
- **Image digest support.** `image: repo@sha256:abc…` deploys by digest, eliminating tag drift. Build-once-promote-many works.
- **Per-service network tiers.** Declare what each service can dial; `yoink` attaches it to those networks at deploy time.
- **Pre-deploy hooks.** Migrations run as one-shot containers (same image, different entrypoint) right before the new runtime container starts.
- **Secrets from Infisical.** Optional. `yoink` invokes the Infisical CLI to fetch the bundle at deploy time and injects into env. Supports rename via `env_from_secrets:`.

## Talking to Docker on remote hosts

`yoink` never installs an agent, daemon, or any code on the target hosts. It opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. The SSH user must be able to read `/var/run/docker.sock` — in practice, root or a member of the `docker` group.

```
yoink preflight    # verifies Docker is reachable on each host
```

> **Important.** Membership in the `docker` group is functionally equivalent to passwordless root on the host. The deploy user's SSH key should be treated as a root credential and scoped accordingly (Tailscale ACLs, `authorized_keys` `from=`, etc.).

## Install

### Homebrew

```
brew install oddur/yoink/yoink
```

### Cargo

```
cargo install --git https://github.com/oddur/yoink yoink
```

### Pre-built binaries

Download from the [latest release](https://github.com/oddur/yoink/releases/latest) — `darwin-arm64`, `darwin-x86_64`, `linux-x86_64`.

## Configuration

One YAML file per project, by convention `yoink.yaml`. Service fragments can be split into per-service files via an `include:` glob. See [`examples/yoink.yaml`](examples/yoink.yaml) and the per-service examples (`backtrack-eu-1-*.yaml`).

Schema highlights:

```yaml
deploy:
  networks: [api, web, redis, otel]      # named tiers — services join the ones they need

hosts:
  - { address: my-server, user: deploy }

secrets:
  provider: infisical
  project_id: <your-infisical-project>
  environment: prod
  domain: https://infisical.example.com  # self-hosted; omit for SaaS

include:
  - services/*.yaml

# services/api.yaml:
services:
  - name: api
    image: ghcr.io/you/api
    # tag: <required at deploy time via --tag api=<sha>, or set here>
    depends_on: [redis]
    networks: [api, redis]
    secrets: [DATABASE_URL]
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }            # mirror the runtime image tag
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]
    run:
      port: 8080
      replicas: 2
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      options:
        memory: 512m
        cap_drop: [ALL]
        read_only: true
        tmpfs: { /tmp: "size=64m,mode=1777" }
        network_aliases: [api]
```

## CLI

```
yoink preflight                          verify Docker is reachable on each host
yoink up [--service NAME]+ [--tag …]+    reconcile to spec (rolling, healthcheck-gated)
yoink prune [--dry-run]                  remove yoink-managed containers no longer in config
yoink rollback SERVICE [--tag <sha>]     redeploy the previous version
yoink history SERVICE [--limit N]        deploy history for a service (newest first)
yoink status [--json]                    snapshot of what's running where
yoink dump                               dense JSON of everything yoink can observe
yoink diff SERVICE [--tag <sha>]         spec diff between live and a target tag
yoink logs SERVICE [--follow] [--tail N] stream container logs
yoink exec SERVICE -- CMD ARGS…          one-shot command inside a running container
yoink shell SERVICE                      interactive PTY shell (k9s-style)
yoink debug SERVICE                      alpine debug sidecar in the target's pid+net ns
yoink restart SERVICE                    bounce a container without re-deploying
yoink kill SERVICE [--yes]               SIGKILL a container; no graceful drain
yoink pull SERVICE [--tag <sha>]         pre-warm an image on the host
yoink networks                           list docker networks across hosts
yoink volumes                            list docker volumes across hosts
yoink top                                htop-style snapshot of CPU/mem per container
yoink version SERVICE                    print currently-running tag(s)
yoink validate                           parse config + check for errors without acting
yoink tui [--mode dashboard|hosts|…]     interactive ratatui dashboard
```

All commands take `--config <path>` (default: `./yoink.yaml`). `-v` / `--verbose` for structured logs to stderr (`info`, `debug`, `trace`).

## TUI

```
yoink tui
```

A keyboard-driven dashboard for the day-to-day. Multiple panes: dashboard (drift across all services), hosts (per-host detail with live CPU/mem), services (per-service detail with replica info), container logs (live tail with `/` filter), per-container detail (env, mounts, healthcheck, networks).

Operator gestures from the dashboard:

- `↑/↓` or `j/k` — navigate
- `enter` — drill into selected row
- `i` — container inspect
- `K` — kill (with confirmation)
- `U` — reconcile this service
- `A` — reconcile all services
- `P` — prune
- `!` — shell into container
- `D` — debug sidecar (alpine in target's pid+net ns)
- `?` — help overlay
- `q` — quit

## Build & test

```bash
cargo build --release           # binary lands at target/release/yoink
cargo test --workspace          # unit + integration tests
cargo clippy --all-targets -- -D warnings
```

A `Taskfile.yml` is included for convenience (`task --list` to see).

## Status

In production use. Driven by a small operator team. Not (yet) widely adopted, no semver-stability promise pre-1.0 — but the core surface (`up`, `status`, `rollback`, the YAML schema) is unlikely to break.

## License

MIT — see [LICENSE](LICENSE).
