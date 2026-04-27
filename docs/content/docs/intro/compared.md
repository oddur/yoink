---
title: Compared to other tools
weight: 2
---

Yoink lives in a populated neighborhood. Pick the tool that fits your scale and operator headcount.

{{< callout type="info" >}}
**Quick decision tree.**
1 host, 1 app → **plain `docker compose`** is fine.
1-3 hosts, Rails/Django/Phoenix-shaped app → **[Kamal](https://kamal-deploy.org)**.
1-10 hosts, multi-service, want drift detection / per-tier networks / k9s-style TUI → **yoink**.
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
| Secrets store | age-sealed `secrets.age` in the repo (default, no external service); Infisical (REST API, no CLI dep) as opt-in | 1Password / Bitwarden / LastPass / generic shell command |
| Accessory containers (postgres, redis, …) | Same shape as services | First-class `accessories:` block |
| Resource limits | k8s-style (`"500m"`, `"2Gi"`) | docker-style (`cpus: 2`, `memory: 1g`) |
| Per-service `pids_limit` | ✓ | ✗ |
| Secure-by-default RunOptions | ✓ (cap_drop=ALL, no-new-privileges, read_only=true, pids_limit=1024, init=tini, tmpfs noexec, binds default :ro) | ✗ (docker defaults) |
| No-registry deploy | ✓ (`yoink up --build --no-registry` runs an ephemeral [unregistry](https://github.com/psviderski/unregistry) sidecar on the host and pushes only the missing layers via SSH; tarball fallback) | ✗ (registry required) |
| Maturity | New (born 2026) | Mature (born 2023, used at 37signals scale) |
| Ecosystem | Rust-built, opinionated for "I run a few heterogeneous services" | Ruby/Rails ecosystem, `rails new` → `kamal init` is the canonical Rails deploy story today |

**Pick Kamal when**: you have a Rails/Phoenix/Hanami app, you want one tool that builds + deploys + routes, and you don't need replicas or per-tier network isolation.

**Pick yoink when**: you've got several services with different shapes (Rust API + Node web + Caddy + Postgres), you want drift detection and a k9s-style TUI for inspect/rollback, and you're willing to bring your own reverse proxy. Or when you want the no-registry, no-CI standalone loop for a hobby project.

## vs. plain `docker compose`

Compose is a YAML schema for declaring a stack on a single host. yoink is a deploy tool that runs against any number of hosts.

| | **yoink** | **docker compose** |
|---|---|---|
| Multi-host | ✓ | ✗ (one host, or you script around it) |
| Healthcheck-gated rolling swap | ✓ | ✗ (`docker compose up -d` recreates without gating) |
| Per-service drift detection | ✓ | ✗ |
| Secure-by-default container options | ✓ | ✗ (compose's defaults are dev-friendly: full caps, writable rootfs, no pids cap) |
| Pre-deploy hooks (migrations etc.) | ✓ (`hooks.pre_deploy`) | partial (`depends_on` + healthcheck dance) |
| Secrets store | ✓ (age-sealed `secrets.age` in the repo by default, Infisical as opt-in) | ✗ (env file or external) |
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
| Secrets management | ✓ (age-sealed in-repo by default, Infisical opt-in) | ✓ (Secrets / external store) |
| Imperative deploy command | `yoink up` | `kubectl apply -f` |

**Pick Kubernetes when**: you have a real infrastructure team, you need autoscaling or multi-tenancy, you're operating across many regions, your engineering org standardized on it, you want a service mesh.

**Pick yoink when**: you want the Kubernetes ideas that pay off at small scale (drift detection, per-tier networks, healthcheck-gated rolls, declarative config) **without** the control plane and the team to operate it.

## vs. nothing — just `ssh` + `docker run`

The honest baseline. Many small deployments are exactly this and that's fine.

**Pick "just ssh + docker run" when**: you have one container, you only need to deploy occasionally, and you don't mind that `docker run` doesn't roll, doesn't healthcheck, doesn't drain, doesn't track who deployed what when.

**Pick yoink when**: you've started writing shell scripts wrapping `ssh + docker run` and you'd like to stop. The yoink config is roughly the shape of those scripts but declarative, and the tool gives you the rolling/healthcheck/drift/history pieces you'd otherwise build yourself.
