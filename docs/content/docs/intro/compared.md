---
title: Compared to other tools
weight: 2
---

Yoink lives in a populated neighborhood. Pick the tool that fits your scale, your operator headcount, and how much always-on infrastructure you want to operate just to run your apps.

{{< callout type="info" >}}
**Quick decision tree.**
1 host, 1 app → **plain `docker compose`** is fine.
1–3 hosts, Rails/Django/Phoenix-shaped app → **[Kamal](https://kamal-deploy.org)**.
1 host, want a Heroku-style "git push to deploy" experience → **[Dokku](https://dokku.com)**.
1+ hosts, want a full self-hosted PaaS with a web UI + build pipelines → **[Coolify](https://coolify.io)**.
1–10 hosts, multi-service, want a web UI + agents on each host → **[Komodo](https://komo.do)**.
1–10 hosts, multi-service, want a built-in WireGuard mesh + per-host daemon → **[Uncloud](https://uncloud.run)**.
1–10 hosts, multi-service, want a single binary with no control plane and no agents — driven equally well by humans, CI, or AI agents → **yoink**.
Many hosts, autoscaling, multi-tenant, "real" infrastructure team → **Kubernetes**.
{{< /callout >}}

## At-a-glance matrix

A high-level cross-section before the per-tool deep dives. ✓ = built-in, ◐ = partial / requires opt-in, ✗ = not in scope.

| | **yoink** | **Kamal** | **Coolify** | **Dokku** | **Komodo** | **Uncloud** | **compose** | **k8s** |
|---|---|---|---|---|---|---|---|---|
| Single binary, no control plane | ✓ | ✓ | ✗ (web app + DB) | ◐ (host-side runtime) | ✗ (Core + per-host agents) | ✗ (per-host daemon) | ✓ | ✗ |
| Always-on services to operate | none | none | Coolify Core + DB | Dokku on each host | Komodo Core + Periphery | `uncloudd` per host | none | etcd + control plane |
| Config-as-code (YAML, in your repo) | ✓ | ✓ | ◐ (UI primary; GitOps opt-in) | ✗ (CLI + buildpacks) | ◐ (UI primary; "Resource Sync" opt-in) | ✓ | ✓ | ✓ |
| **CLI + files only, no GUI required** | ✓ | ✓ | ✗ (UI is the primary surface) | ✓ | ✗ (UI is the primary surface) | ✓ | ✓ | ✓ |
| Agent-friendly (scriptable end-to-end) | ✓ | ✓ | ◐ (REST API exists; UI-first) | ✓ | ◐ (API exists; UI-first) | ✓ | ✓ | ✓ |
| Multiple services per host | ✓ | ◐ (one container per service per host) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Replicas behind a proxy | ✓ | ✗ | ✓ | ✗ | ✓ | ✓ | ◐ | ✓ |
| Multi-host | ✓ | ✓ | ✓ | ✗ (single host) | ✓ | ✓ | ✗ | ✓ |
| Per-tier network isolation | ✓ | ✗ | ◐ | ✗ | ◐ | ◐ | ✓ | ✓ |
| Dependency-ordered deploys | ✓ | ✗ | ✓ | ✗ | ✓ | ✓ | ◐ | ✓ |
| Bundled reverse proxy (HTTPS, ACME) | ✓ (Caddy) | ✓ (`kamal-proxy`) | ✓ (Traefik) | ✓ (nginx) | ✗ | ✓ (Caddy) | ✗ | ◐ (Ingress controller of choice) |
| Healthcheck-gated rolling swap | ✓ | ✓ | ✓ | ◐ | ✓ | ✓ | ✗ | ✓ |
| Drift detection | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ | ✓ |
| Pre-merge dry-run / diff | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (`kubectl diff`) |
| Sealed in-repo secrets (no external service) | ✓ (`age`) | ✗ (external vault required) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| Standalone deploy (no registry, no CI) | ✓ | ✗ | ✗ | ✓ (git push) | ✗ | ✗ | ✓ (local) | ✗ |
| Secure-by-default container options | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ◐ (Pod Security Standards) |
| Port-forward to non-published services | ✓ (`yoink pf`, auto-spawns socat sidecar) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (`kubectl port-forward`) |
| TUI dashboard | ✓ (k9s-style) | ✗ | n/a (web UI) | ✗ | n/a (web UI) | ✗ | ✗ | ◐ (k9s, third-party) |
| One-press rollback | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ | ✓ |
| Auto-scaling | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ |
| Multi-tenancy / RBAC | ✗ | ✗ | ✓ | ◐ | ✓ | ✗ | ✗ | ✓ |

The columns yoink wins on: **single-binary operation, drift detection, sealed in-repo secrets, hardened container defaults, pre-merge diff, standalone (no-registry) mode, port-forwarding to services that don't publish host ports** (a kubectl-style affordance only Kubernetes itself otherwise offers in this peer group), **and one-line templates** (`yoink add postgres`, `yoink add openclaw`, `yoink add gh:acme/templates/foo` — see the [recipe](/docs/recipes/add-templates)). The columns it deliberately doesn't fight on: auto-scaling, multi-tenancy, web UIs.

## vs. [Kamal](https://kamal-deploy.org)

Kamal is the closest neighbor — both are "ship a Rust/Ruby binary, ssh into hosts, drive Docker." The differences:

| Feature | **yoink** | **Kamal** |
|---|---|---|
| Image build | `yoink build [--push]` (optional) | `kamal build` (built-in) |
| Reverse proxy | Bundled Caddy (admin-API push, mTLS, h2c, sealed certs) | First-party `kamal-proxy` |
| Replicas (multiple containers per service) | ✓ (`replicas: N`) | ✗ (one container per service per host) |
| Per-tier networks | ✓ (`networks: [api, redis]`) | ✗ (single shared network) |
| Dependency-ordered deploys | ✓ (`depends_on:` topo-sort, parallel waves) | ✗ |
| Drift detection | ✓ (`yoink.spec_hash` label) | ✗ |
| Pre-merge dry-run / diff | ✓ (`yoink up --dry-run --format=markdown` → sticky PR comment) | ✗ |
| TUI dashboard | ✓ (k9s-style) | ✗ (CLI only) |
| Service deploy history + one-press rollback | ✓ (TUI `H` then `r`) | ✓ (CLI `kamal rollback <version>`) |
| Secrets store | age-sealed `secrets.age` in the repo (default, no external service); `provider: command` shells out to any CLI (1Password `op`, Doppler, Vault, AWS Secrets Manager, Infisical CLI, Bitwarden) | 1Password / Bitwarden / LastPass / generic shell command |
| Accessory / app templates | `yoink add postgres`, `yoink add openclaw`, `yoink add gh:owner/repo/path` — fetched from GitHub, sealed secrets generated, fragment dropped in (poor-man's-helm) | First-class `accessories:` block |
| Resource limits | k8s-style (`"500m"`, `"2Gi"`) | docker-style (`cpus: 2`, `memory: 1g`) |
| Per-service `pids_limit` | ✓ | ✗ |
| Secure-by-default RunOptions | ✓ (cap_drop=ALL, no-new-privileges, read_only=true, pids_limit=1024, init=tini, tmpfs noexec, binds default :ro) | ✗ (docker defaults) |
| Port-forward to non-published services | ✓ (`yoink pf <svc>`, auto-spawns socat sidecar so the locked-down "no `publish:`" default doesn't make debugging awkward) | ✗ (operator BYO `ssh -L`) |
| No-registry deploy | ✓ (`yoink up --build` ships any service with a `build:` block via an ephemeral [unregistry](https://github.com/psviderski/unregistry) sidecar — only the missing layers cross the SSH wire; no registry account needed for those services; tarball fallback) | ✗ (registry required) |
| Driven by AI agents / CI scripts | ✓ (CLI + YAML, identical local & remote) | ✓ (CLI + YAML) |
| Maturity | New (born 2026) | Mature (born 2023, used at 37signals scale) |
| Ecosystem | Rust-built, opinionated for "I run a few heterogeneous services" | Ruby/Rails ecosystem, `rails new` → `kamal init` is the canonical Rails deploy story today |

**Pick Kamal when**: you have a Rails/Phoenix/Hanami app, you want one tool that builds + deploys + routes, and you don't need replicas or per-tier network isolation.

**Pick yoink when**: you've got several services with different shapes (Rust API + Node web + Postgres), you want drift detection, sealed in-repo secrets, and a k9s-style TUI for inspect/rollback. Or when you want the no-registry, no-CI standalone loop for a hobby project.

## vs. [Coolify](https://coolify.io)

Coolify is a self-hosted **PaaS** — think open-source Heroku/Render — running as a long-lived web app on one of your servers. It manages everything: build pipelines, databases, env vars, auto-deploys from git, a UI for non-engineers. Yoink is a much narrower tool that handles the "deploy these containers to these hosts" piece without any of the PaaS surface area.

| | **yoink** | **Coolify** |
|---|---|---|
| Architecture | Single binary, runs on demand | Coolify Core (web app + DB + queue) on one host, agent on each managed host |
| Operator interface | CLI + TUI (keyboard-only) | Web UI (primary), REST API (secondary) |
| State of truth | The git repo's `yoink.yaml` | Coolify's database (UI-edited; environments + projects exported as JSON if you want) |
| Build pipelines | Out of scope (use CI / `yoink build`) | Built-in (Nixpacks, Dockerfile, buildpacks) |
| One-click databases (Postgres / MySQL / Mongo) | Out of scope | Built-in templates with backups |
| Auto-deploy on git push | Out of scope (CI calls `yoink up`) | Built-in (webhook + build) |
| Multi-user, RBAC, audit log | ✗ | ✓ |
| Bundled reverse proxy | ✓ (Caddy) | ✓ (Traefik) |
| Drift detection | ✓ | ✗ |
| **Driven by AI agents / CI scripts** | ✓ (everything is CLI + YAML — no clicking) | ◐ (REST API exists, but UI is the primary surface; agents have to learn the API instead of just editing files) |
| Best fit | Engineers who already have a CI + git workflow and want a thin deploy layer | Teams who want a Heroku-shaped product and are happy operating Coolify itself as a long-lived service |

**Pick Coolify when**: you want batteries-included PaaS (build, deploy, database, web UI, multi-user) and you're comfortable operating Coolify itself as a long-running service on one of your boxes.

**Pick yoink when**: you already have CI + git for the build pipeline, you want the deploy layer to be a thin CLI you run on demand, and you'd rather not run another web app + database to deploy your apps.

## vs. [Dokku](https://dokku.com)

Dokku is the original "Heroku in a box" — a single host runs Dokku, you `git push dokku main`, and a buildpack/Dockerfile flow ships your app behind nginx with auto-TLS. Light on infrastructure, deeply opinionated about the "git push to deploy" loop.

| | **yoink** | **Dokku** |
|---|---|---|
| Architecture | Single binary, runs on demand | Per-host runtime (`dokku` on every server it manages) |
| Number of hosts | Many | One per Dokku install (no native multi-host) |
| Deploy trigger | `yoink up` from operator's laptop or CI | `git push dokku main` |
| Image build | Optional (`yoink build`) or in CI | Built-in (Buildpacks, Dockerfile, Lambda) |
| Bundled reverse proxy | ✓ (Caddy, ACME) | ✓ (nginx, Let's Encrypt plugin) |
| Multiple services per host | ✓ | ✓ (multiple "apps") |
| Replicas | ✓ | ✗ (one container per app process type) |
| Per-tier networks | ✓ | ◐ (Dokku networks plugin) |
| Drift detection | ✓ | ✗ |
| Sealed in-repo secrets | ✓ (`age`) | ✗ (env vars set on the host) |
| Driven by AI agents / CI scripts | ✓ (CLI + YAML, identical local & remote) | ✓ (CLI on the host; SSH to operate remotely) |
| Best fit | Multi-host fleets where the deploy tool is a thin client | Single host you SSH into, "git push to deploy" is the daily loop |

**Pick Dokku when**: one host is enough, the buildpack ergonomics fit your stack, and "git push to deploy" is the user experience you want.

**Pick yoink when**: you want the same "drop a config and run a command" simplicity but across multiple hosts, with replicas, drift detection, and sealed in-repo secrets — and you'd rather not maintain a Dokku install on each box.

## vs. [Komodo](https://komo.do)

Komodo is a fellow Rust-built deploy/management tool for self-hosted containers, with a sizable feature overlap with yoink. The biggest split is **ceremony**: Komodo is a long-running web service with a control plane and per-host agents; yoink is a single binary you run from your laptop or CI.

| | **yoink** | **Komodo** |
|---|---|---|
| Architecture | Single Rust binary, runs on demand | `Komodo Core` (web service + UI) + `Komodo Periphery` (agent on every managed host) |
| Always-on services to operate | None | Two: Core (with a database — MongoDB or FerretDB) + Periphery on each host |
| Operator interface | CLI + TUI (k9s-style, keyboard-only) | Web UI (primary) + CLI/API |
| State of truth | The git repo's `yoink.yaml` | Komodo's database (UI-edited, with optional GitOps "Resource Sync" file imports) |
| Deploy trigger | `yoink up` from operator's laptop or CI | Click in the UI, webhook, scheduled, or sync-from-git |
| Multi-user / RBAC | None — operator-as-user | First-class users, groups, permissions |
| Auth | Whatever ssh + git + your secret store gives you | OAuth (GitHub / Google) + local accounts, baked in |
| Bring-your-own-host | yoink connects to any host you can ssh into | Each host needs `Periphery` installed + reachable from Core |
| Container shape | Yoink-managed containers via per-service spec | Stacks (compose files) / Deployments / Builds / Repos |
| Drift detection | ✓ (`yoink.spec_hash` label) | ✓ (UI shows diff between desired and live) |
| Bundled reverse proxy | ✓ (Caddy) | ✗ (BYO) |
| Healthcheck-gated rolling swap | ✓ | ✓ (compose-style) |
| Audit log of who deployed what | git history of `yoink.yaml` + per-container deploy labels | First-class audit log in the database |
| **Driven by AI agents / CI scripts** | ✓ (CLI + YAML, no GUI in the loop) | ◐ (API exists, but UI is the primary surface) |
| Best fit team size | 1–3 operators sharing a repo | 3+ operators, a team that benefits from a UI + RBAC |

**Pick Komodo when**: you want a UI for non-CLI users, you're managing more hosts than fit in one operator's head, RBAC matters, and the overhead of running a database + a web service + per-host agents is worth it for the control-plane affordances.

**Pick yoink when**: a single binary on your laptop is the entire control surface, the git repo is the source of truth, and you'd rather not operate yet another service-with-database to deploy your services. The CLI/TUI shape works best for 1–3 operators (or for AI agents and CI runners) who are already comfortable in a terminal.

## vs. [Uncloud](https://uncloud.run)

Uncloud is in the same neighborhood as yoink — small fleet, multi-service, deliberate-not-Kubernetes. The biggest split is again **ceremony**: Uncloud installs a per-host daemon (`uncloudd`) on every managed machine and stitches them into a WireGuard mesh; yoink is a one-binary client that talks to docker over plain ssh.

| | **yoink** | **Uncloud** |
|---|---|---|
| Architecture | Single client binary; talks to docker on each host over ssh | Per-host daemon (`uncloudd`) on every managed machine, peer-to-peer (no master) |
| Always-on services to operate | None | One daemon per host (auto-installed by `uncloud machine init`) |
| Cross-host networking | None — yoink relies on docker's per-host networks; cross-host is your routing problem (Tailscale, public DNS, …) | First-class — Uncloud builds a WireGuard mesh between machines, services dial each other by name across hosts |
| Reverse proxy / public ingress | ✓ (bundled Caddy with admin-API push, sealed certs, mTLS) | First-class — built-in Caddy-based ingress with auto-TLS |
| State of truth | The git repo's `yoink.yaml` | Cluster state lives in the daemon mesh; YAML config is also supported |
| Drift detection | ✓ (`yoink.spec_hash` label) | ✓ (compares running state to declared spec) |
| Multi-host service replicas | ✓ (per-host counts, `services[].hosts:` whitelist) | ✓ (cluster-aware scheduling) |
| Onboarding a new host | Already works if you can ssh into it | `uncloud machine init` to install + join the mesh |
| Driven by AI agents / CI scripts | ✓ (CLI + YAML, no daemon to coordinate with) | ✓ (CLI + YAML; daemon-aware) |
| What yoink leaves to other tools that Uncloud bundles | cross-host service-to-service networking | — |
| Best fit | "I have ssh access to my hosts and want a deploy tool that respects that" | "I want a self-hosted lightweight cluster with mesh + ingress baked in" |

**Pick Uncloud when**: cross-host service-to-service networking matters (your api on one box talking to your worker on another), and you're OK with running a daemon per host as the price of that affordance.

**Pick yoink when**: you already have a routing answer (Tailscale, a single edge box, public-DNS-everywhere), you don't want any always-on yoink-specific service running on the host, and the per-host docker daemon over ssh is enough surface area to manage from. The two tools converge on "small fleet of containers, opinionated defaults" — diverge on whether the deploy tool also owns the network fabric.

## vs. plain `docker compose`

Compose is a YAML schema for declaring a stack on a single host. yoink is a deploy tool that runs against any number of hosts.

| | **yoink** | **docker compose** |
|---|---|---|
| Multi-host | ✓ | ✗ (one host, or you script around it) |
| Healthcheck-gated rolling swap | ✓ | ✗ (`docker compose up -d` recreates without gating) |
| Per-service drift detection | ✓ | ✗ |
| Bundled reverse proxy with HTTPS | ✓ (Caddy + ACME / sealed certs) | ✗ (BYO) |
| Secure-by-default container options | ✓ | ✗ (compose's defaults are dev-friendly: full caps, writable rootfs, no pids cap) |
| Port-forward to non-published services | ✓ (`yoink pf <svc>`, auto-spawns socat sidecar) | ✗ (BYO `docker exec` / temporary `ports:` edit) |
| Pre-deploy hooks (migrations etc.) | ✓ (`hooks.pre_deploy`) | partial (`depends_on` + healthcheck dance) |
| Secrets store | ✓ (age-sealed `secrets.age` in the repo by default, `provider: command` for any external CLI) | ✗ (env file or external) |
| TUI / drift dashboard | ✓ | ✗ |
| Driven by AI agents / CI scripts | ✓ | ✓ |
| Single binary, no Python/Compose runtime | ✓ | ✗ (Compose v2 ships with docker, but still a separate runtime) |

**Pick compose when**: dev-machine local stack, single-host hobby project, or you're already comfortable scripting around compose for deploys.

**Pick yoink when**: you want compose's "declarative containers" model with a real deploy story (rolling, multi-host, drift detection, secrets, hardened defaults, bundled proxy).

## vs. Kubernetes

Different leagues. Yoink is for people who don't want a control plane.

| | **yoink** | **Kubernetes** |
|---|---|---|
| Control plane | None | etcd + apiserver + controller-manager + scheduler + kubelet on every node |
| Operator overhead | Single Rust binary on your laptop | A team |
| Auto-scaling | ✗ | ✓ (HPA / VPA / Cluster Autoscaler) |
| Multi-tenant / RBAC | ✗ | ✓ |
| Service mesh, ingress controllers, CSI, CNI plugins | ✗ | The whole ecosystem |
| Replicas + healthcheck-gated rolling swap | ✓ | ✓ (Deployments) |
| Per-tier network isolation | ✓ (named docker networks) | ✓ (NetworkPolicy + CNI) |
| Resource limits (cpu/memory) | ✓ | ✓ |
| Secrets management | ✓ (age-sealed in-repo by default, `provider: command` to any external CLI) | ✓ (Secrets / external store) |
| Port-forward to non-published services | ✓ (`yoink pf <svc>`, auto-spawns socat sidecar) | ✓ (`kubectl port-forward`) |
| Driven by AI agents / CI scripts | ✓ (CLI + one YAML file) | ✓ (kubectl + many YAML files) |
| Imperative deploy command | `yoink up` | `kubectl apply -f` |

**Pick Kubernetes when**: you have a real infrastructure team, you need autoscaling or multi-tenancy, you're operating across many regions, your engineering org standardized on it, you want a service mesh.

**Pick yoink when**: you want the Kubernetes ideas that pay off at small scale (drift detection, per-tier networks, healthcheck-gated rolls, declarative config, in-repo secrets) **without** the control plane and the team to operate it.

## vs. nothing — just `ssh` + `docker run`

The honest baseline. Many small deployments are exactly this and that's fine.

**Pick "just ssh + docker run" when**: you have one container, you only need to deploy occasionally, and you don't mind that `docker run` doesn't roll, doesn't healthcheck, doesn't drain, doesn't track who deployed what when.

**Pick yoink when**: you've started writing shell scripts wrapping `ssh + docker run` and you'd like to stop. The yoink config is roughly the shape of those scripts but declarative, and the tool gives you the rolling/healthcheck/drift/history pieces you'd otherwise build yourself.
