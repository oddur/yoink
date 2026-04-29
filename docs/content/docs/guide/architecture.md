---
title: How it works
weight: 4
---

The mental model for what yoink does under the hood. Useful when an output surprises you and you want to understand why.

## Drift detection

Every yoink-managed container carries a `yoink.spec_hash` label. The hash is computed from a deterministic encoding of the container's **effective spec**:

- `image:tag` (resolved with the operator's `--tag` overrides applied)
- the full env map (literals + every `secrets:` value resolved at deploy time)
- network aliases + the set of attached networks
- mounts: `binds` (after the auto-`:ro` rewrite) + `volumes` + `tmpfs` (after the auto-`noexec,nosuid,nodev` rewrite)
- run options: `memory`, `cpus`, `pids_limit`, `cap_drop`, `cap_add`, `security_opt`, `read_only`, `init`, `user`, `restart`
- ports (`publish` strings)
- `entrypoint` + `cmd`
- the **content hash** of every `files:` mount (so a config-file change reroles the service)

The hash is `sha256` of the encoding, hex-encoded. Two containers with the same `spec_hash` are byte-for-byte the same effective spec.

What's deliberately **not** hashed:
- container name (changes per deploy because it embeds the short hash)
- the `yoink.deployed-by` / `yoink.deployed-at` audit labels
- the host address (the same spec runs on every applicable host)

This is what powers everything that says "this is a no-op" or "this would update":

- `yoink up` short-circuits any service whose desired hash matches the running one — no pull, no swap, no event noise
- `yoink up --dry-run` reports per-service Create / Update / NoOp by computing the desired hash and comparing
- The TUI dashboard's drift column reads the labels off running containers and renders ✓ / ⚠ accordingly
- Two services on different hosts or replicas with matching specs share a hash — drift is per-spec, not per-container

If a `yoink up` rerolls every service when you didn't expect it, the cause is always something that fed into the hash changing. The most common cases: a default flipped between yoink versions (every `v0.x.0` security-defaults change has done this once), a secret value rotated (resolved env changes → hash changes), a `files:` mount's content changed.

## Deploy lock

`yoink up` takes a per-host advisory lock so two operators (or an operator + a CI runner) don't fight over the same docker daemon mid-deploy. The mechanism is a sentinel container, not a file lock — so it survives operator-side crashes cleanly.

How it works:

- On `up`, yoink tries to start a container named `yoink-deploy-lock` on each host. Image: `alpine:latest`, command: a tiny shell loop that watches `/tmp/heartbeat` and exits when the file is older than ~30s.
- If the container is already running on a host, `up` errors with "another deploy in progress." The competing operator either waits or — if the sentinel is stale — re-runs (the sentinel will have self-exited).
- If the container exists but is stopped (a crashed previous deploy), yoink force-removes it and starts fresh. That's the "stopped orphan, sweep + acquire" branch.
- Once acquired, the operator side `docker exec`s into the sentinel every 5 seconds to touch `/tmp/heartbeat`. After ~30s of missed pings the sentinel self-exits.
- On `up` finish (success or error), yoink aborts the heartbeat and force-removes the sentinel. Next deploy: instant acquire.

Failure modes the design handles:

- **Operator crash / SIGKILL / network drop**: heartbeat task dies with the process, sentinel self-exits within 30s, next deploy reaps the stopped orphan.
- **Operator's TUI session leaves a sentinel behind**: same as above — close the session and the sentinel exits within 30s, OR run `yoink lock --release` to reap it explicitly.
- **Two operators race**: whoever's `create_container` lands first wins; the loser's `acquire` errors loudly.

Logs you'll see in the field:

- `lock heartbeat exec failed host=… error=…container is not running` — your local yoink's heartbeat tried to touch a sentinel that someone else swept. Your lock is gone; close + reopen.

## Dependency-ordered deploys

Services declare what they need:

```yaml
services:
  - name: api
    depends_on: [redis]
  - name: web
    depends_on: [api]
  - name: caddy
    depends_on: [api, web]
```

`yoink up` does a topological sort + runs services in **waves**. Within a wave (services whose deps are all satisfied) services run **concurrently** via `try_join_all`. Between waves they run **sequentially**.

For the config above:

- Wave 0: `redis` (no deps)
- Wave 1: `api` (redis is done)
- Wave 2: `web` (api is done)
- Wave 3: `caddy` (api + web are done)

Where it matters: the `pre_deploy` hooks for a service run *before* that service's wave starts, so a database migration completes before the `api` container that depends on it ever pulls.

`yoink validate` (and any deploy command) rejects `depends_on:` cycles at config load time, before yoink touches a host.

## Secrets resolution

Two providers, dispatched by `secrets.provider:`:

- **`age`** (default, batteries-included) — yoink decrypts a sealed `secrets.age` file at deploy time using one X25519 identity, resolved in priority order: `YOINK_AGE_KEY` env (CI), `YOINK_AGE_KEY_FILE` env (explicit override), `~/.config/yoink/keys/<recipient>.key` (laptop default — yoink scans the dir and picks the key whose public matches `secrets.recipients:`), then `~/.config/yoink/age.key` (legacy fallback). `yoink secrets key generate` writes to the keys dir; the first `yoink up` after that finds the right key automatically. For CI, `yoink secrets key generate --print | gh secret set YOINK_AGE_KEY` pipes the secret into your store, and the workflow surfaces it as `YOINK_AGE_KEY` at deploy time. See [Sealed secrets (age)](/docs/guide/secrets) for the full mental model.
- **`command`** — yoink invokes the configured command, captures stdout, and parses it as a secrets bundle. Format is auto-detected: stdout starting with `{` is JSON, anything else is dotenv (`KEY=value\n`). One spawn per `yoink up` (and once on TUI startup). Stderr is captured and surfaced when the command exits non-zero.

There's no first-party integration with any specific manager. Operators wire their tool of choice via its standard CLI: `doppler secrets download --format env`, `infisical export --format=dotenv`, `vault kv get -format=json`, `aws secretsmanager get-secret-value`, etc. See [external secrets via CLI](/docs/guide/secrets) for per-tool recipes.

Both providers produce the same shape: a key→value bundle. Every service that lists `secrets:` (or `env_from_secrets:`) gets its values picked out of that bundle and injected as env vars on the container — which means the values feed into `yoink.spec_hash`, which is why a rotated secret triggers a redeploy.

## Prune semantics

`yoink prune` removes containers that match all three:

1. Carry the `yoink.managed=true` label (so we never touch unmanaged containers)
2. Are **not** declared in the current config — either the service was renamed, removed, or this is a stale generation from a previous deploy
3. Are **not** the active sentinel container for an in-flight deploy (the lock survives prune)

What `prune` does **not** remove:
- Containers without the `yoink.managed=true` label (legacy containers, pre-yoink hand-runs, etc.)
- Stopped containers that *are* in the current config (they might be part of a paused deploy, or `yoink up` will reap them on the next reconcile)
- Volumes or networks (use `docker volume prune` / `docker network prune` for those)

`yoink prune --dry-run` prints what would be removed without acting. Run it before merging a service rename — the staging container with the old name shows up in the list.

## Runtime container shape

When yoink creates a container, the docker host config it sends is built from `service.run.options` plus a few fixed pieces:

- **Auto-add to `tmpfs` mount options**: `noexec,nosuid,nodev` (unless operator explicitly opted in to `exec`/`suid`/`dev`)
- **Auto-add to `binds`**: `:ro` suffix when no mode set (operator opts in to `:rw` explicitly)
- **`init: true`** by default — tini as PID 1 reaps zombies + forwards SIGTERM
- **Network mode** = first network from the merged service+deploy `networks:` list; additional networks attached post-start
- **Port bindings** parsed from `publish:` entries
- **Restart policy** = the `restart:` string (`no` / `always` / `unless-stopped` / `on-failure`); default unset = no restart

The full security-defaults table is on [Secure by default](/docs/guide/security).

## Healthcheck-gated rolling swap

For each replica of each service, yoink does:

1. Resolve the desired tag, build the spec, compute `spec_hash`.
2. Check the host snapshot — if a container with the matching name + `spec_hash` is already running, skip (no-op).
3. Otherwise: pull the image (or skip when an earlier `load_images_to_hosts` phase already shipped the build artifact, or when `--no-registry` forced local-only mode), create the new container with the resolved name (`<service>-<short_hash>` or `<service>-<short_hash>-<idx>` for replicas), start it.
4. Probe the configured healthcheck (`healthcheck_path` HTTP GET, or a TCP-connect probe on `port` if no path is set). Retry on a backoff until it passes or `healthcheck_timeout` elapses.
5. On healthy: stop the previous-generation container (waiting `drain_timeout` for graceful shutdown), then force-remove it.
6. On healthcheck failure: leave the new container running but exited, leave the old one running, surface the error. The operator inspects via `yoink logs` / `yoink shell` and either fixes config or rolls back.

Replicas run sequentially (one container at a time per replica index) so capacity stays at N-1 during the swap. Across services in the same wave, swaps run concurrently.
