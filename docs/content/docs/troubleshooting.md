---
title: Troubleshooting
weight: 6
---

Error messages you might see, what they mean, and how to fix them.

## `docker save exited with status 1`

A service has a `build:` block (so yoink wants to ship it from local) but the image isn't built locally yet.

```sh
yoink up --build        # build first, then ship
```

Or build in a separate step: `yoink build` then `yoink up`.

## `lock heartbeat exec failed: container not found`

A previous `yoink up` (or TUI deploy) was killed without cleaning up. The sentinel container is gone but the lock label persists, and the heartbeat task can't find its container to update the timestamp.

The lock auto-expires after ~30s of stale heartbeat. Wait, then retry. If it persists, manually remove the sentinel:

```sh
ssh deploy@<host> 'docker rm -f yoink-lock'
```

## `yoink up` / `yoink tui` hangs silently with no output

Almost always an interactive ssh prompt yoink can't surface — yoink's docker-over-SSH transport spawns its own `ssh` client and swallows stderr, so when ssh prompts for an extra check the connection just blocks.

Run `yoink preflight` to get a classified error. The two common cases:

**Tailscale SSH "additional check required":**
```
✗ host-1: ssh probe failed: Tailscale SSH requires an additional
  check — open this URL in a browser, then re-run:
    https://login.tailscale.com/a/<token>
```

Open the URL, approve the device, re-run the command. (Tailscale SSH gates new sessions through a browser tap when ACLs require it.)

**Permission denied:**
```
✗ host-x: ssh probe failed: permission denied — check the ssh key
  (`ssh-add -l`) and that the deploy user is configured on the host
```

Add the key with `ssh-add ~/.ssh/id_ed25519` (or whichever) and re-run.

`yoink preflight` is the canonical "why can't I reach the host" diagnostic — it does an explicit `ssh -o BatchMode=yes user@host true` first and translates common stderr patterns. The classifier covers Tailscale auth, permission denied, connection timeout, host key changes, DNS / hostname resolution, and connection refused.

For the "host is provisioning right now and Docker isn't installed yet" race (common on `cloud-init`-driven first boots), `yoink preflight --wait 90s` polls each host on backoff (2s → 15s capped) until Docker responds or the budget lapses. Saves writing your own `until ssh … cloud-init status --wait` loop before the first `yoink up`.

## `` `<command>` exited with status N: <stderr> ``

`provider: command` ran your secrets binary and it failed. The captured stderr is included in the error — start there.

Common causes:

- The CLI needs a session/token env var that isn't set on this runner (e.g. `DOPPLER_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`, `VAULT_TOKEN`). Run the same command manually to confirm.
- The CLI is missing on the deploy runner (operator's laptop has it via Homebrew; CI doesn't). Install it in the workflow before `yoink up`.
- The configured project / vault / secret name has changed. Re-check the argv against your secret manager's UI.

## `parse secrets bundle from \`<command>\` (treated as <format>)`

The command exited 0 but yoink couldn't parse stdout. Either:

- The output was empty (the CLI silently filtered to zero secrets — usually a permissions issue at the source).
- A value legitimately starts with `{`, tripping the `auto` JSON heuristic. Set `format: dotenv` (or `format: json`) explicitly in `yoink.yaml`.
- The dotenv stream contains a malformed line (no `=`). Yoink fails loud rather than silently dropping malformed lines; the error names the line number.

## `image_present=false; pull failed: 404 not found`

Your CI hasn't built and pushed this SHA yet. Either:

- Wait for the build workflow to finish, or
- Pass a different `--tag <service>=<sha>` that exists, or
- Add a `build:` block to the service and pass `--build` so yoink ships the local image directly instead of pulling.

## `secrets.NAME not allowed in step-level if:` (GitHub Actions)

GitHub Actions doesn't expand `secrets.*` inside `if:` expressions. Move the gate up to the job level, or compute a boolean in an earlier step and gate on its output.

## `service "X" missing required tag`

The image entry has no `tag:` and no `--tag X=...` was passed. Either set a literal `tag:` in the config, or pass it at deploy time.

## `cycle detected in depends_on: a -> b -> a`

`depends_on` must be a DAG. Break the cycle — usually one of the edges is wrong (e.g. a sidecar that should depend on its parent, not vice versa).

## `network "X" referenced by service but not declared`

Service-level `networks: [X]` requires `X` to be in `deploy.networks` (or to be the default `deploy.network`). Add it.

## `healthcheck timed out after 60s`

The new container started but `/health` didn't return 200 within `healthcheck_timeout`. Common causes:

- App is genuinely slow to boot — bump `healthcheck_timeout`.
- Wrong `healthcheck_path` — yoink probes HTTP `GET <path>` on the published port; some apps expose `/healthz` or `/readyz` instead of `/health`.
- App is crashing — `yoink logs <service>` (or the TUI logs view) shows the container's stderr.

The old container stays live; the new one is rolled back. No traffic loss.

## `pre_deploy hook X failed (exit 1)`

The hook ran (good — it found the image) but exited non-zero. Yoink aborts the deploy before swapping the runtime container, so the old version stays live.

`yoink logs --hook X` shows the hook's stderr. Common cases: failing migration, missing migration secret (different role from the runtime), schema drift the migration didn't expect.

## `permission denied: /var/run/docker.sock` on the host

The deploy user isn't in the `docker` group on the target host:

```sh
ssh root@<host> 'usermod -aG docker deploy'
```

Log out and back in (or restart the SSH session) for the group change to take effect.

## TUI shows containers but `yoink up` says "no changes"

Working as intended. [`spec_hash`](/docs/guide/architecture#drift-detection) matches — nothing to do. If you expected a redeploy, something in the input didn't actually change. To force one, bump a label:

```yaml
labels:
  redeploy_nonce: "2026-04-26"
```

## See also

- [Architecture](/docs/guide/architecture) — drift detection, deploy lock, healthcheck-gated swap.
- [CLI reference](/docs/reference/cli) — every flag and subcommand, including the diagnostic ones (`yoink doctor`, `yoink validate`, `yoink status`).
- [Configuration reference](/docs/reference/config) — schema for everything in `yoink.yaml`.
