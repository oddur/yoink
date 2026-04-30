---
title: Glossary
description: "Definitions for cross-cutting terms: spec_hash, unregistry, provider:age, yoink-ingress, and more."
weight: 4
---

Cross-cutting terms. Each entry is one paragraph with a link to the page that covers the concept in depth.

## Deploy engine

**`spec_hash`.** A `sha256` over a deterministic encoding of every field that defines a container's runtime: image+tag, env (including resolved secrets), networks, mounts, run options, ports, entrypoint+cmd, plus the content hash of any `files:` mount. Yoink labels every container it creates with its `spec_hash`. Two containers with the same `spec_hash` are byte-for-byte the same effective spec. See [Architecture → Drift detection](/docs/guide/architecture#drift-detection).

**Drift detection.** Yoink computes the desired `spec_hash` from the config, compares it against the running container's label, and skips the swap when they match. Drives `yoink up`'s "no changes" output, the dry-run diff, and the TUI's drift indicators.

**Reconcile / `yoink up`.** Brings the running fleet to match the config: pull, create, healthcheck, swap, drain, reap. Idempotent. The TUI's `U` key runs the same code path.

**Healthcheck-gated swap.** Yoink starts the new container, runs its healthcheck (`healthcheck_path` HTTP probe or TCP-connect on `port`), and only stops the old container after the new one passes. A failed healthcheck leaves the old container serving; the operator gets the error. See [Architecture → Rolling deploy](/docs/guide/architecture#rolling-deploy).

**Deploy lock.** A sentinel container yoink creates on each host before reconciling. A second `yoink up` against the same host blocks until the first releases. Force-release via `yoink lock release` after a crashed session. See [Architecture → Deploy lock](/docs/guide/architecture#deploy-lock).

**Waves / dependency-ordered.** Services with `depends_on:` get topo-sorted into waves; swaps inside a wave run concurrently. The api waits for redis to be healthy before its own swap.

**Pre-deploy hook.** A one-shot container under `hooks.pre_deploy:` (top-level) or `services[].pre_deploy:` (per-service). Runs once per `up` (not per replica), before the swap of the service it's attached to. Migrations pattern: same image as the runtime, different entrypoint.

## Build origin and distribution

**`build:` block.** A service with `build:` is built on the operator's machine before deploy. Without it, the image is pulled from a registry. Mixed configs work — some services build, others pull. See [Deploy modes](/docs/guide/deploy-modes).

**Standalone mode.** `yoink up --build` ships an image from the operator's docker daemon to each host over SSH. Default transport is unregistry (layer dedup); falls back to tarball on hosts that can't pull the unregistry sidecar. See [Standalone (no-registry) deploys](/docs/how-to/standalone-mode).

**Unregistry.** An ephemeral [OCI registry sidecar](https://github.com/psviderski/unregistry) yoink starts on each host during standalone deploys. Reads/writes the host's image store directly via the containerd socket — no separate blob storage, layer-level dedup over the SSH tunnel. Removed on the next deploy.

**Tarball transport.** Standalone fallback when unregistry can't run on the host (air-gapped). Streams `docker save` from operator to host's docker daemon over SSH. No layer dedup.

## Secrets

**`provider: age`.** Default secrets path. A `secrets.age` file committed to git, sealed against one or more age recipients, decrypted at deploy time. Two-line setup. See [Secrets](/docs/guide/secrets#sealed-secrets-age--the-default).

**`provider: command`.** Bring-your-own-tool secrets path. Yoink invokes the configured command, parses dotenv or JSON from stdout, feeds the result to the same machinery `provider: age` uses. Wire any secret manager (sops, Doppler, Infisical, Vault, AWS SM, 1Password, Bitwarden). See [External secrets via CLI](/docs/guide/secrets#external-secrets-via-cli-provider-command).

**Recipient vs. identity.** The two halves of an age keypair. The **recipient** (`age1…`, public) lives in `secrets.recipients:` and seals new values — safe to commit. The **identity** (`AGE-SECRET-KEY-1…`, private) lives in your secret manager and unseals at deploy time — never commit.

**`YOINK_AGE_KEY`.** The env var yoink reads at deploy time to find the age identity. Set it in CI as a secret; on a laptop, yoink discovers the key from `~/.config/yoink/keys/<recipient>.key`. See [Identity resolution](/docs/guide/secrets#identity-resolution).

**Sealed bundle.** The encrypted dotenv file (default name: `secrets.age`) holding every secret value. Sealed against the recipients in `yoink.yaml`. Edit via `yoink secrets edit`; consume via `services[].secrets:` and `services[].env_from_secrets:`.

## Reverse proxy

**`yoink-proxy` / bundled Caddy.** A managed Caddy service yoink injects when any service declares `domain:`. Routes hostnames, terminates TLS, issues Let's Encrypt certs, rolls with the services it fronts. Same drift detection, logs, and TUI surface as any other yoink service. See [Reverse proxy](/docs/guide/proxy).

**`domain:`.** The field that puts a service behind the proxy. `domain: api.example.com` injects `yoink-proxy` into the deploy and routes the hostname to the container.

**`tls: auto` / `cert` / `off`.** Per-service TLS mode. `auto` (default) provisions a Let's Encrypt cert via HTTP-01. `cert` uses an inline cert from a sealed-secret pair (`tls_cert_secret` + `tls_key_secret`) — typical for Cloudflare Origin Certificates. `off` serves on `:80` only.

**`proxy.xcaddy:`.** A list of Caddy plugins (Go modules) yoink compiles into a custom Caddy binary on each proxy host via an `xcaddy` build sidecar. Used for rate-limiting, the L4 module, third-party DNS providers, shared ACME storage. See [Caddy plugins](/docs/how-to/caddy-plugins).

**`yoink-ingress` network.** The docker network yoink joins both the proxy and every service with a `domain:` to. Caddy reaches upstream containers by name without bridging docker networks. Services keep their other networks on top.

**`yoink-proxy-admin` network.** A proxy-private network for Caddy's admin API. Yoink pushes new config over an SSH-tunneled forward on this network without restarting Caddy.

## Networking

**`publish:` (and the no-publish default).** Yoink never adds `publish:` automatically. A service without `publish:` is reachable only on its docker networks — never bound to a host port. See [Networking → Port-forward](/docs/guide/networking#port-forward).

**`yoink pf <service>`.** Tunnels from your laptop to a container port over the SSH connection yoink already uses. Works regardless of `publish:`.

**Sidecar (in `yoink pf` context).** When the target has no `publish:`, `yoink pf` spawns an `alpine/socat` container on the host, joined to the service's docker network, forwarding the container port through the SSH tunnel. Removed on Ctrl-C.

**`services[].hosts:`.** Per-service host pin. A service runs on every host in `hosts:` by default; `hosts: [prod-eu-1]` pins it to one. Used for stateful singletons and region-specific shapes. See [Networking → Multi-host distribution](/docs/guide/networking#multi-host-distribution).

## Hosts and config

**Host fragment.** A YAML file under `hosts/<name>.yaml` (or any path matched by an `include:` glob) declaring one or more `hosts:` entries. `yoink hosts add` writes one of these for each new host instead of editing your authored `yoink.yaml`.

**`ssh_key_secret:`.** A field on a host entry naming a sealed-secret entry holding a PEM SSH private key. Yoink decrypts the key into a per-process tempfile for SSH instead of using the operator's ssh-agent. Pair with `yoink secrets ssh-key generate --seal-as <NAME>`.

**`address_secret:`.** A field on a host entry naming a sealed-secret entry holding the host's address (IP or hostname). Resolved at config-load time from the same sealed bundle that feeds `ssh_key_secret:`. Use when the deploy config sits in a public repo and the host's address itself is sensitive. Mutually exclusive with `address:`. Pair with `yoink hosts add --address-secret <KEY> --user <USER> --name <NAME>`.

**`include:` glob.** Top-level list of file globs relative to `yoink.yaml`. Each matched file is parsed as a fragment and its `services:`, `hosts:`, and `hooks.pre_deploy:` lists merge into the main config.

## Templates

**`yoink add <name>`.** Fetches a template from GitHub, prompts for variables, generates and seals declared secrets, and renders files into your project. Same code path for bundled templates (`yoink add postgres`) and 3rd-party (`yoink add gh:owner/repo/name`). See [Adding a service via `yoink add`](/docs/how-to/using-templates).

**Template manifest.** The `template.yaml` inside a template directory. Declares variables (prompts, defaults, choices, regex), files to render (minijinja → operator's repo), secrets to generate-and-seal, and an optional `include_glob` for the main config. See [Authoring templates](/docs/guide/templates).

**`gh:` ref form.** A 3rd-party template reference: `gh:owner/repo[@ref]/path`. `ref` is a branch, tag, or SHA (defaults to `main`). Resolved to a SHA once and cached content-addressed.

## Substitution

**`${VAR}` env-var expansion.** Substitutes against the process environment at config-load time. For per-deploy values like a host IP that isn't known until provisioning. See [Configuration → Environment-variable substitution](/docs/reference/config#environment-variable-substitution).

**`${VAR:-default}` fallback.** POSIX-style: substitutes the text after `:-` when the var is unset. For commands that parse the config but don't use the value (e.g. `yoink secrets seal` against a config referencing `${HOST_IP}`).

## See also

- [Configuration reference](/docs/reference/config) — every field in `yoink.yaml`.
- [CLI reference](/docs/reference/cli) — every subcommand and flag.
- [Architecture](/docs/guide/architecture) — the deploy engine.
