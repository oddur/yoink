# yoink

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. Sits between Kamal and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.


```
yoink up                  # reconcile every service in dep order
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

## Why it exists

Kamal is a lovely fit for "one app, one binary per host" but starts to creak the moment you want a second app on the same box, replicas of the same service, or any kind of network isolation between containers. Kubernetes solves all that — and a hundred other problems you don't have, in exchange for a control plane to operate, a YAML schema with a learning curve, and a vocabulary you have to teach every new operator.

`yoink` is the thinnest tool that gives you the Kubernetes ideas that actually matter at small scale, while keeping Kamal's "one binary, ssh into the host, drive Docker directly" simplicity:

- **Multiple services per host.** Declare them, deploy them, prune the ones that fall out of config.
- **Replicas.** Want two `api` containers behind a reverse proxy? Set `replicas: 2`. Healthcheck-gated rolling swap.
- **Per-service network tiers.** Each service joins a named network; only services on the same network can dial each other. Basic blast-radius isolation without a CNI plugin.
- **Dependency-ordered deploys.** Services declare `depends_on:` and `yoink up` runs them in topological-sort waves (independent services in parallel). No more "I redeployed `api` before `redis` was on the new network".
- **Drift detection.** Every effective spec (image, env, networks, mounts, options, file content) hashes deterministically and lands as a label. The TUI shows drift across the cluster without guessing.
- **k9s-style TUI.** A `ratatui` dashboard with one-key reconcile, prune, kill, shell-into, debug-sidecar, log filter. Keyboard-only. Survives offline and slow links because it's just talking docker over ssh.

What it deliberately doesn't do:

- No control plane, no agent on the hosts. yoink is a single Rust binary on your laptop or in CI.
- No service discovery, no scheduling — Docker's network alias DNS is plenty for a few services on a host.
- No multi-cluster, no HA, no auto-scaling. One operator, one config, one deploy at a time.
- No reverse proxy, secret store, load balancer, or stateful-accessory orchestration. Use the right tool for each.

## How it pairs with Tailscale + caddy-docker-proxy

`yoink` doesn't try to solve "how do I reach my hosts" or "how do I route HTTPS to the right container". It expects you to bring two off-the-shelf tools that solve those completely:

### Tailscale for SSH (the connectivity layer)

yoink's transport is `ssh://user@host` via [bollard's SSH transport](https://docs.rs/bollard/), which opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. Pairing this with **Tailscale SSH**:

- **Hostnames work everywhere.** MagicDNS gives every host a stable name (`my-server`) reachable from your laptop, CI runner, anywhere on the tailnet. No `ssh_config` to maintain, no jump hosts, no bastion.
- **Auth without keys.** Tailscale SSH issues short-lived certs based on tailnet membership and ACLs. Onboard a new operator: invite to the tailnet, grant ACL access to the `tag:server` group. Done. Off-board: revoke from tailnet. Their key is gone everywhere, immediately.
- **CI authentication is the same flow.** A GH Actions runner with `tailscale/github-action` joins the tailnet under `tag:ci`; ACL grants `tag:ci → tag:server` SSH; yoink connects without ever touching `~/.ssh/`.
- **No port forwarding.** The Docker daemon never listens on a TCP port. The SSH transport handles auth + transport in one hop. Surface area: `:22` accessible only from the tailnet.

The deploy user on each host is in the `docker` group (functionally root, scope your tailnet ACLs accordingly).

### caddy-docker-proxy for the public surface

yoink owns the container lifecycle. **It does not own routing.** Public traffic landing on your hosts wants:

- TLS termination (with auto-renewing certs)
- Hostname → container routing
- Per-route headers, redirects, rate limits

[caddy-docker-proxy](https://github.com/lucaslorentz/caddy-docker-proxy) is a Caddy plugin that watches the Docker socket for container labels and reconfigures itself live. The pairing:

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    labels:
      caddy: api.example.com
      caddy.reverse_proxy: "{{upstreams 8080}}"
    run:
      port: 8080
      replicas: 2
      networks: [public, api]
```

`yoink up` deploys the api container with those labels; the caddy container (also yoink-managed, but separately) reads them via the docker socket and routes `https://api.example.com` to the api containers, automatically picking up new replicas and dropping retired ones. Cert issuance is Caddy's job (Let's Encrypt or Cloudflare origin); yoink doesn't know it's happening.

Replicas plug into this naturally: `caddy.reverse_proxy: "{{upstreams 8080}}"` resolves all containers with the same network alias and round-robins between them. yoink's healthcheck-gated rolling swap means the caddy upstream pool is always traffic-ready.

The split:

| concern | tool |
|---|---|
| **container lifecycle** (pull, start, healthcheck, drain, replace) | yoink |
| **dep-ordered deploys** (redis before api before caddy) | yoink |
| **per-tier network isolation** | yoink |
| **HTTPS / hostname routing / cert renewal** | caddy-docker-proxy |
| **operator → host connectivity** | Tailscale |
| **CI → host connectivity** | Tailscale |
| **stateful services** (postgres, etc.) | docker compose on the host |
| **secrets** | Infisical (REST API, no CLI install needed) |

## Running the same stack twice on the same hosts (staging alongside prod)

Common scenario: you want a `staging` environment that mirrors `prod` for pre-merge verification, ideally on the same hardware to avoid paying for a second machine. yoink doesn't have a built-in `environment` concept — instead you use **two config files** that don't collide.

The two things that have to differ between envs:

1. **Service names** — Docker container names are unique per host. `api` and `api-staging` can both run; `api` and `api` cannot.
2. **Network names** — `api` (prod tier) and `api-staging` (staging tier) keep traffic separated even though both stacks live on the same docker daemon.

Everything else (volumes, hostnames, secrets, ports) follows from those two namespaces.

### Layout

```
yoink.prod.yaml             # prod entry; includes services/prod/*.yaml
yoink.staging.yaml          # staging entry; includes services/staging/*.yaml
services/prod/api.yaml
services/prod/web.yaml
services/staging/api.yaml   # name: api-staging, networks: [api-staging, redis-staging, ...]
services/staging/web.yaml   # name: web-staging
```

Each config is operated independently:

```bash
yoink up --config yoink.prod.yaml          # prod deploy
yoink up --config yoink.staging.yaml       # staging deploy
yoink dump --config yoink.staging.yaml     # staging snapshot
yoink tui --config yoink.staging.yaml      # TUI scoped to staging containers
```

The TUI's drift detection, `prune`, and reconcile are all scoped to whatever `--config` references — `yoink prune --config staging.yaml` won't touch prod containers because their `yoink.service` labels don't match anything declared in the staging config.

### Public surface (caddy-docker-proxy)

The reverse proxy is the one shared piece of infra: caddy serves both envs from the same `:443` listener. Routing is by hostname:

- `api.example.com` → containers labeled with that hostname (the prod api)
- `staging-api.example.com` → containers labeled with the staging hostname (the staging api)

Use the host network alias to keep it clean:

```yaml
# services/staging/api.yaml
- name: api-staging
  image: ghcr.io/you/api
  labels:
    caddy: staging-api.example.com
    caddy.reverse_proxy: "{{upstreams 8080}}"
  run:
    network_aliases: [api-staging]    # different alias from prod's `api`
```

caddy-docker-proxy picks both up and routes by `Host:` header. No collision.

### Stateful accessories (redis, postgres, etc.)

Two options, pick per service:

- **Separate instances per env**: `redis` and `redis-staging` containers, both yoink-managed, on their own networks. Cheap; full isolation.
- **Shared instance, namespaced data**: one `redis`, but staging uses key prefix `staging:` (app-side responsibility). Half the memory; no data isolation.

For SQL — typically a managed Postgres with separate databases (`myapp` and `myapp_staging`) under the same cluster. Yoink doesn't deploy the database; the connection strings live in env vars / secrets per env.

### Tag overrides

Staging usually wants to deploy whatever's on `main`, while prod tracks `live`. Either:

- Keep `tag:` empty in the service yaml; supply via `--tag api-staging=$MAIN_SHA` from CI when deploying staging
- Or commit a digest pointer in `yoink.staging.yaml` (build-once-promote-many) — same image, different deploy targets

The image identity is the same across envs; only the runtime configuration differs.

## Three deploy modes

Yoink supports three ways of getting a container image to a host. Pick the one that fits your setup; they're not mutually exclusive (mix per-service or per-environment).

| | Image origin | Build | Distribution | When it fits |
|---|---|---|---|---|
| **CI-built (default)** | CI (GitHub Actions, Depot, etc.) builds + pushes on every commit | external | `docker pull` from registry | Production with multiple hosts and an existing CI/CD pipeline |
| **Local-build, kamal-style** | Operator's machine | `yoink build --push` | `docker pull` from registry | Indie / one-person team that wants the full deploy loop in one tool, willing to keep a registry |
| **Standalone (no registry)** | Operator's machine | `yoink build` | `docker save \| docker load` over ssh | Rapid iteration on a single host; air-gapped; "I just want this thing running" |

Pick by service if you want — `yoink build` only runs against services with a `build:` block, so a config can mix CI-built infrastructure (`caddy`, `redis`) with locally-built app code.

### CI-built — the default

Operator config has no `build:` block; CI builds the image and pushes it to a registry on every merge. `yoink up` pulls and rolls. This is what every example so far in this README has been doing — `image: ghcr.io/you/api` with `--tag api=<sha>` overriding tag at deploy time.

### Local-build, kamal-style

```yaml
services:
  - name: api
    image: ghcr.io/you/api          # real registry path
    tag: dev
    build:                          # tells `yoink build` how to produce the image
      context: .
      dockerfile: Dockerfile
      args:
        RUST_VERSION: "1.95"
    run:
      port: 8080
```

```sh
docker login ghcr.io                 # one-time
yoink build api --push               # docker build → docker push ghcr.io/you/api:dev
yoink up --service api               # docker pull on each host → rolling swap
```

`yoink build --push` runs `docker build` against your local docker daemon, tagging as `<image>:<tag>` (which equals the registry path because of how `image:` is configured), then runs `docker push`. After that the standard deploy path takes over — exactly like kamal, except the build step is a separate command (so you can build without pushing for CI of your own; or push without rebuilding for re-tagging).

Two-step instead of one-shot is deliberate: `yoink build && yoink up` is the explicit version of `kamal deploy`, and the separation lets you run them on different machines (build on a beefy laptop, deploy from a thin runner) when you want to.

### Standalone (no registry)

```yaml
services:
  - name: api
    image: my-app                   # bare name — no registry prefix
    tag: dev
    build:
      context: .
    run:
      port: 8080
```

```sh
yoink build api                     # docker build → tag local image as my-app:dev
yoink up --no-registry --service api # save+load to each host (no docker pull)
```

What `--no-registry` does: for every (service, host) the deploy targets, yoink streams `docker save <image>:<tag>` from the operator's local docker daemon directly into the host's docker daemon via the same ssh+bollard transport that `up` already uses (calling `POST /images/load`). The transfer is **streaming** — chunks of ~8 KiB flow through the pipe, never buffering the full tarball in operator-side RAM. A 2 GB image costs 8 KiB of memory on the operator's machine, not 2 GB. After the load completes, the host has the image cached and the rest of the deploy flow (which already short-circuits when an image is locally present) runs unchanged — healthcheck-gated rolling swap, drift detection, the works.

Per-host progress bars track bytes transferred + rate live:

```
⠋ api:dev → backtrack-eu-1     234.5 MiB @  47.0 MiB/s
⠋ api:dev → backtrack-eu-2      56.0 MiB @  11.2 MiB/s
✓ web:dev → backtrack-eu-1     done · 89.3 MiB
```

Multi-host fan-out is fully concurrent (`try_join_all` per image): each host gets its own `docker save` process + its own bollard connection, so deploy time = max(per-host) instead of sum(per-host).

**Standalone mode fits when**:

- Rapid iteration on a single host. Edit Dockerfile → `yoink build api && yoink up --no-registry --service api` → see it running on the box. ~15 seconds for a small image. No registry to set up, no CI to wait on.
- Hobby / indie / prototype deployments. "I just want this thing on a server" — no build pipeline, no registry account.
- Air-gapped or restricted-network hosts. The host doesn't need outbound network access to anything but the operator's ssh.

**It doesn't fit when**:

- Many hosts. N hosts = N save+load streams. A registry is a hub that lets each host pull the missing layers in parallel from a closer point. For 1-3 hosts the difference is small; at 10+ hosts a registry is meaningfully faster.
- Large images. Every deploy ships the full image tarball over ssh per host. A 200MB Rust binary is fine; a 2GB Java or Node app gets uglier each deploy.
- You want rollback by tag. `--no-registry` mode relies on the host's local image cache for older generations; `docker image prune` blows that away. A registry keeps every pushed tag indefinitely.
- Auditability matters. A registry has a record of "image `<sha256:…>` was pushed at time T." `--no-registry` is invisible outside the local docker daemons.

### Self-hosted registry as a yoink service

The middle ground between "real remote registry" and "no registry at all": run `registry:2` as a yoink-managed service on one of your hosts, expose it via tailscale, point `image:` at the tailnet hostname.

```yaml
services:
  - name: registry
    image: registry
    tag: "2"
    networks: [registry]
    run:
      port: 5000
      volumes:
        - "registry-data:/var/lib/registry"
      options:
        memory: "256Mi"

  - name: api
    image: registry.my-tailnet.ts.net:5000/api
    tag: dev
    build:
      context: .
```

Then `yoink build api --push` pushes to your tailnet registry; `yoink up --service api` pulls from it. Tailscale ACLs are the registry's auth surface — no `docker login` / registry password needed if your tailnet is correctly scoped. Persistent storage via the `registry-data` volume.

Best for "I want a registry but I don't want to pay for one and I don't want to run it on a separate machine."

Recipe — not yoink-specific code. Best for "I want a registry but I don't want to pay for one and I don't want to run it on a separate machine."

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

One YAML file per project, by convention `yoink.yaml`. Service fragments can be split per-service via an `include:` glob. See [`examples/`](examples/) for the full shape.

```yaml
deploy:
  networks: [public, api, redis, otel]   # named tiers — services join the ones they need

hosts:
  - { address: my-server, user: deploy } # reachable via tailnet hostname

secrets:
  provider: infisical                    # optional; yoink talks to Infisical's REST API directly
  project_id: <your-infisical-project>
  environment: prod
  # domain: https://infisical.example.com  # only for self-hosted Infisical instances

include:
  - services/*.yaml

# services/api.yaml:
services:
  - name: api
    image: ghcr.io/you/api
    # tag: <required at deploy time via --tag api=<sha>, or set here>
    depends_on: [redis]
    networks: [api, redis]               # can dial only services on these networks
    secrets: [DATABASE_URL]
    pre_deploy:
      - name: api-migrate
        image: ghcr.io/you/api
        tag: { service: api }            # mirror the runtime tag
        cmd: ["migrate"]
        secrets: [DATABASE_MIGRATE_URL]
    run:
      port: 8080
      replicas: 2
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 30s
      options:
        memory: "512Mi"
        cpus: "1.5"
        # cap_drop / security_opt / read_only / pids_limit all default
        # to the hardened profile (see "Sane security defaults" below).
        # Just give the app some scratch space:
        tmpfs: { /tmp: "size=64m,mode=1777" }
        network_aliases: [api]           # caddy-docker-proxy upstream key
```

### Sane security defaults

Compose's defaults are dev-friendly, not prod-friendly: every container runs with the full Linux capability set, a writable rootfs, no `pids` cgroup limit, no protection against setuid escalation. Yoink flips those — every service yoink creates is hardened by default. **The only knobs that opt *into* less safety are explicit:**

| field | yoink default | what it gives you | how to opt out |
|---|---|---|---|
| `cap_drop` | `["ALL"]` | every Linux capability dropped (no `CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …) | `cap_drop: []` (full default cap set), or surgically re-add via `cap_add` |
| `security_opt` | `["no-new-privileges:true"]` | setuid binaries inside the container can't escalate via `execve` | `security_opt: []` |
| `read_only` | `true` | rootfs mounted read-only — exploits can't drop binaries on disk | `read_only: false` |
| `tmpfs` mount opts | auto-`noexec,nosuid,nodev` | a writable scratch tmpfs can't be used to drop + run a binary, set setuid bits, or create device nodes | include explicit `exec`/`suid`/`dev` in your tmpfs option string |
| `binds` mode | `:ro` when no mode set | accidental bind-mount-and-write to host paths is impossible without explicit `:rw` | `"/host:/container:rw"` (explicit) |
| `init` | `true` | tini as PID 1 reaps zombies + forwards SIGTERM, so drains and rolling swaps actually finish | `init: false` for images that ship their own init (systemd-in-containers, s6, supervisord) |
| `pids_limit` | `1024` | bounds the fork-bomb class of exploits | `pids_limit: null` (unlimited), or any positive int |
| `cpus` | `null` (uncapped) | — | set to `"2"`, `"500m"`, `"1.5"`, etc. — k8s style |
| `memory` | `null` (uncapped) | — | set to `"512Mi"`, `"1Gi"`, `"512m"`, `"1g"` — k8s + docker styles both accepted |

Surgical opt-outs in practice (from a real prod config):

```yaml
# caddy needs to bind :80/:443 — re-add NET_BIND_SERVICE only.
- name: caddy
  run:
    cap_drop: [ALL]
    cap_add:  [NET_BIND_SERVICE]
    cpus: "0.5"
    memory: "64Mi"

# pgadmin's image scribbles all over its rootfs at startup — back out
# of read_only, keep the rest of the hardened defaults.
- name: pgadmin
  run:
    read_only: false
    cpus: "1"
    memory: "256Mi"

# Image with stubborn legacy code that needs root + writable /etc.
- name: legacy-thing
  run:
    user: "0:0"
    read_only: false
    cap_drop: []
    security_opt: []
```

**Read-only rootfs in practice**: the app still needs *somewhere* to write — `/tmp`, sometimes `/run`, occasionally a cache directory. Pair `read_only: true` with `tmpfs:` to give it scratch space without letting it persist:

```yaml
run:
  read_only: true
  tmpfs:
    /tmp: "size=64m,mode=1777"
    /var/cache/app: "size=128m,mode=0755"
```

What yoink **doesn't** do automatically (for now): seccomp/AppArmor profiles beyond docker's defaults, user-namespace remapping (`--userns-remap`), gVisor / Kata runtime selection. Those are host-wide concerns, not per-service config — set them on the docker daemon and yoink containers inherit.

The TUI's container detail (`i` from any container row) shows the effective security profile for each running container under "security & limits" — easy to spot when something's accidentally lax.

### Authenticating with Infisical

When `secrets.provider: infisical` is set, yoink resolves a bearer token in this order:

1. **Universal Auth** (machine identity) — `INFISICAL_CLIENT_ID` + `INFISICAL_CLIENT_SECRET`. The CI path; create a machine identity in Infisical's UI and expose the pair as repo secrets.
2. **Raw bearer token** — `INFISICAL_TOKEN`. For one-off runs where you already have a token in hand.
3. **Cached browser-flow login** — yoink reads the session that the `infisical` CLI persists after `infisical login`. Run it once on your laptop:
   ```sh
   brew install infisical/get-cli/infisical
   infisical login                        # add --domain=… for self-hosted
   ```
   yoink then reuses that session — the CLI binary itself is not invoked at deploy time, only its keychain entry is read.

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

All commands take `--config <path>` (default: `./yoink.yaml`). `-v` for structured logs to stderr.

## TUI

```
yoink tui
```

<img width="1435" height="789" alt="image" src="https://github.com/user-attachments/assets/bcf956b1-937a-43d7-92f6-0f24a1d0cd98" />

<img width="2862" height="1820" alt="image" src="https://github.com/user-attachments/assets/36d3cc00-355c-482b-addc-454a0070b58e" />



Heavily inspired by [k9s](https://k9scli.io/). Keyboard-driven panes for dashboard (drift across all services), hosts (per-host detail with live CPU/mem), services (per-service detail with replica info), container logs (live tail with `/` filter), per-container detail (env, mounts, healthcheck, networks).

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
- `H` — service deploy history; on a stopped row press `r` to roll back
- `?` — help overlay
- `q` — quit

### Pretty logs

Structured log lines (JSON, logfmt, etc.) are hard to scan as raw text. The TUI's logs pane auto-detects [`hl`](https://github.com/pamburus/hl) (`brew install pamburus/tap/hl`) on the operator's `PATH` and transparently pipes every container's log stream through it before rendering — so JSON keys are colored, timestamps are dim, levels are highlighted, and stack traces stay readable. Falls back to raw output when `hl` isn't installed; no config knob to toggle.

The `yoink logs <svc> -f` CLI doesn't auto-pipe (the operator decides their own shell pipeline), but `yoink logs api -f | hl` works the same way.

## Build & test

```bash
cargo build --release                # binary lands at target/release/yoink
cargo test --workspace               # unit + integration tests
cargo clippy --all-targets -- -D warnings
```

## Status

In production use on a small fleet. Pre-1.0 and not (yet) widely adopted, so no semver-stability promise — but the core surface (`up`, `status`, `rollback`, the YAML schema) is unlikely to break.

## License

MIT — see [LICENSE](LICENSE).
