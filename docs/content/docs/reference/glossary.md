---
title: Glossary
description: "Definitions of the terms that occur across the docs, for example spec_hash, unregistry, provider: age, and yoink-ingress."
weight: 4
---

## Deploy engine

**`spec_hash`.** A `sha256` over a deterministic encoding of every field that defines the runtime of a container. The fields are image+tag, env (with resolved secrets), networks, mounts, run options, ports, entrypoint+cmd, and the content hash of each `files:` mount. yoink labels every container that it creates with its `spec_hash`. See [Architecture → Drift detection](/docs/explanation/architecture#drift-detection).

**Drift detection.** yoink computes the desired `spec_hash` from the config and compares it with the label of the running container. When the two match, yoink skips the swap. Drift detection drives the "no changes" output of `yoink up`, the dry-run diff, and the drift indicators in the TUI.

**Reconcile / `yoink up`.** Makes the running fleet match the config: pull, create, healthcheck, swap, drain, reap. The operation is idempotent. The `U` key in the TUI runs the same code path.

**Healthcheck-gated swap.** yoink starts the new container and runs its healthcheck (the `healthcheck_path` HTTP probe, or a TCP-connect on `port`). yoink stops the old container only after the new one passes. After a failed healthcheck, yoink removes every container that the failed attempt created for the service on that host. The old containers keep running. yoink prints the last 30 log lines of the failed container and writes them to the audit log as `DeployFailed`. See [Architecture → Rolling deploy](/docs/explanation/architecture#healthcheck-gated-rolling-swap).

**Deploy lock.** A sentinel container that yoink creates on each host before it reconciles. While the lock is held, a second `yoink up` against the same host stops with an error. After a crashed session, `yoink lock release` removes the lock by force. See [Architecture → Deploy lock](/docs/explanation/architecture#deploy-lock).

**Waves / dependency-ordered.** yoink sorts services with `depends_on:` topologically into waves. The services in one wave deploy concurrently. For example, the api waits until redis is healthy before its own swap.

**Pre-deploy hook.** A one-shot command under `hooks.pre_deploy:` (top-level) or `services[].pre_deploy:` (per-service). It runs in a container, or as a subprocess on your computer. It runs once for each `up` (not once for each replica), before the swap of the service that it is attached to. The pattern for migrations is the same image as the runtime with a different entrypoint.

## Build origin and distribution

**`build:` block.** yoink builds a service that has `build:` on your computer before the deploy. Without `build:`, yoink pulls the image from a registry. One config can contain services of the two types. See [Deploy modes](/docs/explanation/deploy-modes).

**Standalone mode.** `yoink up --build` sends an image from your docker daemon to each host over SSH. The default transport is unregistry (layer dedup). If unregistry cannot start on a host, the transport is the tarball. See [Standalone (no-registry) deploys](/docs/how-to/standalone-mode).

**Unregistry.** An ephemeral [OCI registry sidecar](https://github.com/psviderski/unregistry) that yoink starts on each host during standalone deploys. It reads and writes the image store of the host directly through the containerd socket. It has no separate blob storage, and it gives layer-level dedup over the SSH tunnel. yoink removes it after the push. The next deploy removes a sidecar that a crash left behind.

**Tarball transport.** The standalone fallback when unregistry cannot run on the host (for example, an air-gapped host). It streams `docker save` from your computer to the docker daemon of the host over SSH. It has no layer dedup.

## Secrets

**`provider: age`.** The default secrets path. A `secrets.age` file is committed to git and sealed against one or more age recipients. yoink decrypts it at deploy time. See [Secrets](/docs/explanation/secrets#sealed-secrets-age).

**`provider: command`.** The bring-your-own-tool secrets path. yoink runs the configured command and parses dotenv or JSON from stdout. Any secret manager with a CLI can connect this way (sops, Doppler, Infisical, Vault, AWS SM, 1Password, Bitwarden). See [External secrets via CLI](/docs/explanation/secrets#external-secrets-via-cli-provider-command).

**Recipient vs. identity.** The two halves of an age keypair. The **recipient** (`age1…`, public) is in `secrets.recipients:` and seals new values. It is safe to commit. The **identity** (`AGE-SECRET-KEY-1…`, private) is in your secret manager and unseals at deploy time. Do not commit it.

**`YOINK_AGE_KEY`.** The env var that yoink reads first to find the age identity. In CI, it is a secret. On a laptop, yoink finds the key in `~/.config/yoink/keys/<recipient>.key`. See [Identity resolution](/docs/explanation/secrets#identity-resolution).

**Sealed bundle.** The encrypted dotenv file (default name: `secrets.age`) that holds every secret value. It is sealed against the recipients in `yoink.yaml`. `yoink secrets edit` edits it. `services[].secrets:` and `services[].env_from_secrets:` consume it.

## Reverse proxy

**`yoink-proxy` / bundled Caddy.** A managed Caddy service that yoink injects when a service declares `domain:`. It routes hostnames, terminates TLS, and issues Let's Encrypt certs. It has the same drift detection, logs, and TUI surface as any other yoink service. See [Reverse proxy](/docs/explanation/proxy).

**`domain:`.** The field that puts a service behind the proxy. `domain: api.example.com` injects `yoink-proxy` into the deploy and routes the hostname to the container.

**`tls: auto` / `cert` / `off`.** The TLS mode of one service. `auto` (default) provisions a Let's Encrypt cert through HTTP-01. `cert` uses an inline cert from a sealed-secret pair (`tls_cert_secret` + `tls_key_secret`), for example a Cloudflare Origin Certificate. `off` serves on `:80` only.

**`proxy.xcaddy:`.** A build config with a list of Caddy plugins (Go modules). yoink compiles them into a custom Caddy binary on each proxy host. Examples are rate-limiting, the L4 module, third-party DNS providers, and shared ACME storage. See [Caddy plugins](/docs/how-to/caddy-plugins).

**`yoink-ingress` network.** The docker network that yoink attaches the proxy and every service with a `domain:` to. Caddy reaches upstream containers by name on this network. Services keep their other networks.

**`yoink-proxy-admin` network.** A network private to the proxy, for the admin API of Caddy. The admin API listens only on the address of the proxy on this network. yoink pushes new config to the admin API through an SSH tunnel and the admin relay, and does not restart Caddy.

**Admin relay.** The `yoink-proxy-admin-relay` container on each proxy host. It joins only `yoink-proxy-admin`, publishes the admin port on `127.0.0.1` of the host, and sends each connection to the admin API of the proxy.

## Networking

**`publish:` (and the no-publish default).** yoink never adds `publish:` automatically. A service without `publish:` is reachable only on its docker networks and is never bound to a host port. See [Networking → Port-forward](/docs/explanation/networking#port-forward).

**`yoink pf <service>`.** Opens a tunnel from your laptop to a container port over the SSH connection that yoink already uses. It works with or without `publish:`.

**Sidecar (in `yoink pf` context).** When the target has no `publish:`, `yoink pf` starts an `alpine/socat` container on the host. The container joins the docker network of the service and forwards the container port through the SSH tunnel. Ctrl-C removes it.

**`services[].hosts:`.** The host pin of one service. By default, a service runs on every host in `hosts:`. `hosts: [prod-eu-1]` pins it to one host, for example for a stateful singleton. See [Networking → Multi-host distribution](/docs/explanation/networking#multi-host-distribution).

## Hosts and config

**Host fragment.** A YAML file under `hosts/<name>.yaml`, or at any path that an `include:` glob matches, that declares one or more `hosts:` entries. `yoink hosts add` writes one fragment for each new host and does not edit the `yoink.yaml` that you wrote.

**`ssh_key_secret:`.** A field on a host entry. It names a sealed-secret entry that holds a PEM SSH private key. yoink decrypts the key into a per-process tempfile for SSH and does not use your ssh-agent. The related command is `yoink secrets ssh-key generate --seal-as <NAME>`.

**`address_secret:`.** A field on a host entry, mutually exclusive with `address:`. It names a sealed-secret entry that holds the address of the host (IP or hostname). yoink resolves it at config-load time. It is for a deploy config in a public repo when the address of the host is sensitive. The related command is `yoink hosts add --address-secret <KEY> --user <USER> --name <NAME>`.

**`include:` glob.** A top-level list of file globs, relative to `yoink.yaml`. yoink parses each matched file as a fragment. The `services:`, `hosts:`, and `hooks.pre_deploy:` lists of the fragment merge into the main config.

## Templates

**`yoink add <name>`.** Fetches a template from GitHub and prompts for variables. Then it generates and seals the declared secrets and renders files into your project. Bundled templates (`yoink add postgres`) and third-party templates (`yoink add gh:owner/repo/name`) use the same code path. See [Adding a service via `yoink add`](/docs/how-to/using-templates).

**Template manifest.** The `template.yaml` in a template directory. It declares variables (prompts, defaults, choices, regex) and the files to render (minijinja → your repo). It also declares the secrets to generate and seal, and an optional `include_glob` for the main config. See [Authoring templates](/docs/explanation/templates).

**`gh:` ref form.** A reference to a third-party template: `gh:owner/repo[@ref]/path`. `ref` is a branch, tag, or SHA (the default is `main`). yoink resolves it to a commit SHA and caches the template by that SHA.

## Substitution

**`${VAR}` env-var expansion.** Substitutes against the process environment at config-load time. It is for per-deploy values, for example a host IP that is not known before provisioning. See [Configuration → Environment-variable substitution](/docs/reference/config#environment-variable-substitution).

**`${VAR:-default}` fallback.** POSIX style: substitutes the text after `:-` when the var is unset. It is for commands that parse the config but do not use the value. An example is `yoink secrets seal` against a config that references `${HOST_IP}`.

## See also

- [Configuration reference](/docs/reference/config): every field in `yoink.yaml`.
- [CLI reference](/docs/reference/cli): every subcommand and flag.
- [Architecture](/docs/explanation/architecture): how the deploy engine works.
- [Standalone (no-registry) deploys](/docs/how-to/standalone-mode): the procedure that uses unregistry and the tarball transport.
