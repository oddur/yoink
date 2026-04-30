---
title: Glossary
weight: 4
---

Cross-cutting terms that show up across the docs. Each entry is one paragraph with a link to the page that covers the concept in depth — useful when you land on a how-to or recipe and hit a term that hasn't been introduced.

## Deploy engine

**`spec_hash`.** A `sha256` over a deterministic encoding of every field that defines a container's runtime: image+tag, env (including resolved secrets), networks, mounts, run options, ports, entrypoint+cmd, plus the content hash of any `files:` mount. Yoink labels every container it creates with its `spec_hash`. Two containers with the same `spec_hash` are byte-for-byte the same effective spec. See [Architecture → Drift detection](/docs/guide/architecture#drift-detection).

**Drift detection.** Yoink computes the desired `spec_hash` from the config, compares it against the running container's label, and skips the swap when they match (no-op) or rolls when they differ. This is what powers `yoink up`'s "no changes" output, the dry-run diff, and the TUI's drift indicators.

**Reconcile / `yoink up`.** The engine that takes the config as ground truth and brings the actual fleet to match it: pull, create, healthcheck, swap, drain, reap. Idempotent — `yoink up` on a clean fleet is a no-op. The same code path drives the TUI's `U` key.

**Healthcheck-gated swap.** Yoink starts the new container, runs its healthcheck (`healthcheck_path` HTTP probe or TCP-connect on `port`), and only stops the old container after the new one passes. A broken build can't replace a working one — the new container exits, the old one keeps serving, the operator gets a clear error. See [Architecture → Rolling deploy](/docs/guide/architecture#rolling-deploy).

**Deploy lock.** A sentinel container yoink creates on each host before reconciling. Two operators (or two CI jobs) running `yoink up` against the same host will see the second one block until the first releases. Force-release via `yoink lock release` when an operator's session crashed mid-deploy. See [Architecture → Deploy lock](/docs/guide/architecture#deploy-lock).

**Waves / dependency-ordered.** Services with `depends_on:` declarations get topo-sorted into waves; within a wave, swaps run concurrently across services. The api waits for redis to be healthy before its own swap starts.

**Pre-deploy hook.** A one-shot container declared under `hooks.pre_deploy:` (top-level) or `services[].pre_deploy:` (per-service). Runs once per `up` (not per replica), before the swap of the service it's attached to. The migrations pattern: same image as the runtime, different entrypoint.

## Build origin and distribution

**`build:` block.** When a service declares `build:`, yoink runs `docker build` on the operator's machine before deploying. Without it, the image comes from a registry. Mixed configs are fine — some services build locally, others pull. See [Deploy modes](/docs/guide/deploy-modes).

**Standalone mode.** `yoink up --build` ships an image straight from the operator's docker daemon to each host over SSH — no registry needed. Default transport is unregistry (layer dedup); falls back to tarball on hosts that can't pull the unregistry sidecar. See [Standalone (no-registry) deploys](/docs/how-to/standalone-mode).

**Unregistry.** An ephemeral [OCI registry sidecar](https://github.com/psviderski/unregistry) yoink starts on each host during standalone deploys. Reads/writes the host's image store directly via the containerd socket — no separate blob storage, layer-level dedup over the SSH tunnel. Cleaned up on the next deploy.

**Tarball transport.** The standalone fallback when unregistry can't run on the host (truly air-gapped). Streams `docker save` from operator to host's docker daemon over SSH. No layer dedup; whole image crosses the wire every time.

## Secrets

**`provider: age`.** Yoink's default secrets path. A single `secrets.age` file committed to git, sealed against one or more age recipients, decrypted at deploy time. Two-line setup. See [Secrets](/docs/guide/secrets#sealed-secrets-age--the-default).

**`provider: command`.** Yoink's bring-your-own-tool secrets path. Yoink invokes the configured command, parses dotenv or JSON from stdout, feeds the result to the same machinery that `provider: age` uses. Operators wire whatever secret manager they already run (sops, Doppler, Infisical, Vault, AWS SM, 1Password, Bitwarden, …). See [External secrets via CLI](/docs/guide/secrets#external-secrets-via-cli-provider-command).

**Recipient vs. identity.** The two halves of an age keypair. The **recipient** (`age1…`, public) lives in `secrets.recipients:` in `yoink.yaml` and seals new values — safe to commit. The **identity** (`AGE-SECRET-KEY-1…`, private) lives in your secret manager and unseals at deploy time — never commit. The asymmetry is what makes the sealed file safe to put in git.

**`YOINK_AGE_KEY`.** The env var yoink reads at deploy time to find the age identity. Set it in CI as a secret; on a laptop, yoink discovers the key from `~/.config/yoink/keys/<recipient>.key` instead. See [Identity resolution](/docs/guide/secrets#identity-resolution).

**Sealed bundle.** The encrypted dotenv file (default name: `secrets.age`) that holds every secret value. Sealed against the recipients in `yoink.yaml`. Edit via `yoink secrets edit`; consume via `services[].secrets:` and `services[].env_from_secrets:`.

## Reverse proxy

**`yoink-proxy` / bundled Caddy.** A managed Caddy service yoink auto-injects when any service declares `domain:`. Routes hostnames, terminates TLS, issues Let's Encrypt certs, and rolls cleanly with the services it fronts. The Caddy is a normal yoink-managed service — same drift detection, same logs, same TUI. See [Reverse proxy](/docs/guide/proxy).

**`domain:`.** The field that puts a service behind the proxy. Setting `domain: api.example.com` on a service is what auto-injects `yoink-proxy` into the deploy and routes the hostname to the container.

**`tls: auto` / `cert` / `off`.** Per-service TLS mode. `auto` (default) provisions a Let's Encrypt cert via HTTP-01. `cert` uses an inline cert from a sealed-secret pair (`tls_cert_secret` + `tls_key_secret`) — typical for Cloudflare Origin Certificates. `off` serves on `:80` only, no HTTPS.

**`proxy.xcaddy:`.** A list of Caddy plugins (Go modules) yoink builds into a custom Caddy binary on each proxy host. No registry needed — yoink runs an `xcaddy` build sidecar to compile, then runs the resulting binary. Used for rate-limiting, the L4 module, third-party DNS providers, shared ACME storage, etc. See [Caddy plugins](/docs/how-to/caddy-plugins).

**`yoink-ingress` network.** The docker network yoink creates and joins both the proxy and every service with a `domain:` to. That's how Caddy reaches upstream containers by name without bridging across docker networks. Services keep their other networks (e.g. `api ↔ redis`) on top.

**`yoink-proxy-admin` network.** A second proxy-private network for Caddy's admin API. Yoink reaches the admin API via SSH-tunneled forward over this network — that's how it pushes new config without restarting Caddy.

## Networking

**`publish:` (and the no-publish default).** Yoink doesn't add `publish:` entries automatically. A service without `publish:` is reachable only on its docker networks (other yoink services, the proxy) — never bound to a host port. The locked-down default. See [Networking → Port-forward](/docs/guide/networking#port-forward).

**`yoink pf <service>`.** Tunnels from your laptop to a container port over the same SSH connection yoink already uses. Works whether or not the service publishes. The kubectl-style affordance that makes "no published ports by default" practical for debugging.

**Sidecar (in `yoink pf` context).** When the target service has no `publish:`, `yoink pf` spawns a tiny `alpine/socat` container on the host, joined to the service's docker network, forwarding the container port back through the SSH tunnel. Cleaned up on Ctrl-C. The "make a no-publish service reachable without changing the deploy posture" trick.

**`services[].hosts:`.** Per-service host pin. By default a service runs on every host in `hosts:`; with `hosts: [prod-eu-1]` it only runs on that one. Used for stateful singletons (a redis pinned to one host) and region-specific shapes. See [Networking → Multi-host distribution](/docs/guide/networking#multi-host-distribution).

## Hosts and config

**Host fragment.** A YAML file under `hosts/<name>.yaml` (or any directory matched by an `include:` glob) that declares one or more `hosts:` entries. `yoink hosts add` writes one of these for each new host so yoink never has to round-trip-edit your authored `yoink.yaml`.

**`ssh_key_secret:`.** A field on a host entry naming a sealed-secret entry that holds a PEM SSH private key. Yoink decrypts the key into a per-process tempfile and uses it for SSH instead of the operator's ssh-agent. Pair with `yoink secrets ssh-key generate --seal-as <NAME>`.

**`include:` glob.** Top-level `include:` list of file globs (relative to `yoink.yaml`'s directory). Each matched file is parsed as a fragment and its `services:`, `hosts:`, and `hooks.pre_deploy:` lists are merged into the main config. The pattern that lets one project carry many small files instead of one wall.

## Templates

**`yoink add <name>`.** Fetches a vetted template from GitHub, prompts for variables, generates and seals any declared secrets, and renders the rendered files into your project. Same code path serves bundled templates (`yoink add postgres`) and 3rd-party (`yoink add gh:owner/repo/name`). See [Adding a service via `yoink add`](/docs/how-to/using-templates).

**Template manifest.** The `template.yaml` file inside a template's directory: declares variables (with prompts, defaults, choices, regex patterns), files to render (minijinja → operator's repo), secrets to generate-and-seal, and an optional `include_glob` to wire into the operator's main config. See [Authoring templates](/docs/guide/templates).

**`gh:` ref form.** The shape of a 3rd-party template reference: `gh:owner/repo[@ref]/path`, where `ref` is a branch, tag, or SHA (defaults to `main`). Resolved to a SHA once and cached content-addressed.

## Substitution

**`${VAR}` env-var expansion.** Any `${VAR}` reference in `yoink.yaml` (or an included fragment) is substituted against the process environment at config-load time. Useful for per-deploy values like a host IP that isn't known until provisioning. See [Configuration → Environment-variable substitution](/docs/reference/config#environment-variable-substitution).

**`${VAR:-default}` fallback.** POSIX-style default: substitutes the literal text after `:-` when the var is unset. Useful for commands that parse the config but don't actually use the value (e.g. `yoink secrets seal` against a config whose hosts/domains reference `${HOST_IP}`).

## See also

- [Configuration reference](/docs/reference/config) — every field that shows up in `yoink.yaml`.
- [CLI reference](/docs/reference/cli) — every subcommand and flag.
- [Architecture](/docs/guide/architecture) — the deploy engine in detail.
