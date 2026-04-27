---
title: Compared to other tools
weight: 2
---

Yoink lives in a populated neighborhood. Pick the tool that fits your scale and operator headcount.

{{< callout type="info" >}}
**Quick decision tree.**
1 host, 1 app → **plain `docker compose`** is fine.
1-3 hosts, Rails/Django/Phoenix-shaped app → **[Kamal](https://kamal-deploy.org)**.
1-10 hosts, multi-service, want a web UI + agents on each host → **[Komodo](https://komo.do)**.
1-10 hosts, multi-service, want a single binary with no control plane → **yoink**.
Many hosts, autoscaling, multi-tenant, "real" infrastructure team → **Kubernetes**.
{{< /callout >}}

## vs. [Kamal](https://kamal-deploy.org)

Kamal is the closest neighbor — both are "ship a Rust/Ruby binary, ssh into hosts, drive Docker." The differences:

| Feature | **yoink** | **Kamal** |
|---|---|---|
| Image build | `yoink build [--push]` (optional) | `kamal build` (built-in) |
| Reverse proxy | None — pairs with caddy-docker-proxy / Traefik | First-party `kamal-proxy` |
| Replicas (multiple containers per service) | ✓ (`replicas: N`) | ✗ (one container per service per host) |
| Per-tier networks | ✓ (`networks: [api, redis]`) | ✗ (single shared network) |
| Dependency-ordered deploys | ✓ (`depends_on:` topo-sort, parallel waves) | ✗ |
| Drift detection | ✓ (`yoink.spec_hash` label) | ✗ |
| Pre-merge dry-run / diff | ✓ (`yoink up --dry-run --format=markdown` → sticky PR comment) | ✗ |
| TUI dashboard | ✓ (k9s-style) | ✗ (CLI only) |
| Service deploy history + one-press rollback | ✓ (TUI `H` then `r`) | ✓ (CLI `kamal rollback <version>`) |
| Secrets store integration | Infisical (REST API, no CLI dep) | 1Password / Bitwarden / LastPass / generic shell command |
| Accessory containers (postgres, redis, …) | Same shape as services | First-class `accessories:` block |
| Resource limits | k8s-style (`"500m"`, `"2Gi"`) | docker-style (`cpus: 2`, `memory: 1g`) |
| Per-service `pids_limit` | ✓ | ✗ |
| Secure-by-default RunOptions | ✓ (cap_drop=ALL, no-new-privileges, read_only=true, pids_limit=1024, init=tini, tmpfs noexec, binds default :ro) | ✗ (docker defaults) |
| No-registry deploy | ✓ (`yoink up --build --no-registry` streams `docker save` over ssh) | ✗ (registry required) |
| Maturity | New (born 2026) | Mature (born 2023, used at 37signals scale) |
| Ecosystem | Rust-built, opinionated for "I run a few heterogeneous services" | Ruby/Rails ecosystem, `rails new` → `kamal init` is the canonical Rails deploy story today |

**Pick Kamal when**: you have a Rails/Phoenix/Hanami app, you want one tool that builds + deploys + routes, and you don't need replicas or per-tier network isolation.

**Pick yoink when**: you've got several services with different shapes (Rust API + Node web + Caddy + Postgres), you want drift detection and a k9s-style TUI for inspect/rollback, and you're willing to bring your own reverse proxy. Or when you want the no-registry, no-CI standalone loop for a hobby project.

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
| Reverse proxy | None — pairs with caddy / Traefik | None — same |
| Healthcheck-gated rolling swap | ✓ | ✓ (compose-style) |
| Audit log of who deployed what | git history of `yoink.yaml` + per-container deploy labels | First-class audit log in the database |
| Best fit team size | 1–3 operators sharing a repo | 3+ operators, a team that benefits from a UI + RBAC |

**Pick Komodo when**: you want a UI for non-CLI users, you're managing more hosts than fit in one operator's head, RBAC matters, and the overhead of running a database + a web service + per-host agents is worth it for the control-plane affordances.

**Pick yoink when**: a single binary on your laptop is the entire control surface, the git repo is the source of truth, and you'd rather not operate yet another service-with-database to deploy your services. The CLI/TUI shape works best for 1–3 operators who are already comfortable in a terminal.

## vs. plain `docker compose`

Compose is a YAML schema for declaring a stack on a single host. yoink is a deploy tool that runs against any number of hosts.

| | **yoink** | **docker compose** |
|---|---|---|
| Multi-host | ✓ | ✗ (one host, or you script around it) |
| Healthcheck-gated rolling swap | ✓ | ✗ (`docker compose up -d` recreates without gating) |
| Per-service drift detection | ✓ | ✗ |
| Secure-by-default container options | ✓ | ✗ (compose's defaults are dev-friendly: full caps, writable rootfs, no pids cap) |
| Pre-deploy hooks (migrations etc.) | ✓ (`hooks.pre_deploy`) | partial (`depends_on` + healthcheck dance) |
| Secrets store integration | ✓ (Infisical native) | ✗ (env file or external) |
| TUI / drift dashboard | ✓ | ✗ |
| Single binary, no Python/Compose runtime | ✓ | ✗ (Compose v2 ships with docker, but still a separate runtime) |

**Pick compose when**: dev-machine local stack, single-host hobby project, or you're already comfortable scripting around compose for deploys.

**Pick yoink when**: you want compose's "declarative containers" model with a real deploy story (rolling, multi-host, drift detection, secrets, hardened defaults).

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
| Secrets management | ✓ (Infisical) | ✓ (Secrets / external store) |
| Imperative deploy command | `yoink up` | `kubectl apply -f` |

**Pick Kubernetes when**: you have a real infrastructure team, you need autoscaling or multi-tenancy, you're operating across many regions, your engineering org standardized on it, you want a service mesh.

**Pick yoink when**: you want the Kubernetes ideas that pay off at small scale (drift detection, per-tier networks, healthcheck-gated rolls, declarative config) **without** the control plane and the team to operate it.

## vs. nothing — just `ssh` + `docker run`

The honest baseline. Many small deployments are exactly this and that's fine.

**Pick "just ssh + docker run" when**: you have one container, you only need to deploy occasionally, and you don't mind that `docker run` doesn't roll, doesn't healthcheck, doesn't drain, doesn't track who deployed what when.

**Pick yoink when**: you've started writing shell scripts wrapping `ssh + docker run` and you'd like to stop. The yoink config is roughly the shape of those scripts but declarative, and the tool gives you the rolling/healthcheck/drift/history pieces you'd otherwise build yourself.
