---
title: Troubleshooting
description: Common error messages, their causes, and the steps to correct them.
weight: 6
---

## `` `docker save` for "<image>" exited with status 1 ``

A service has a `build:` block, so yoink ships the image from your computer. The image does not exist locally.

Build the image and ship it in one command:

```sh
yoink up --build        # build first, then ship
```

As an alternative, run `yoink build`, then run `yoink up`.

## `lock heartbeat exec failed: container not found`

yoink holds a deploy lock on each host as a sentinel container, `yoink-deploy-lock`. During a deploy, yoink sends a heartbeat to the sentinel every 5 seconds. This warning shows that one heartbeat did not arrive.

One warning is harmless. The sentinel stops after 30 seconds without a heartbeat, and the next deploy removes a stopped sentinel automatically.

If the next deploy stops with `lock container yoink-deploy-lock is already running on <host> (another deploy in progress)`:

1. Make sure that no other deploy runs. Wait 30 seconds, then try again.
2. If the error continues, remove the sentinel:

   ```sh
   yoink lock release --yes
   ```

## `yoink up` / `yoink tui` hangs silently with no output

The usual cause is an interactive ssh prompt that yoink cannot show. The docker-over-SSH transport starts its own `ssh` client and discards stderr. When ssh prompts for an extra check, the connection blocks.

Run `yoink preflight` to get a classified error. `yoink preflight` first runs `ssh -o BatchMode=yes user@host true` and translates these stderr patterns:

- Tailscale auth
- permission denied
- connection timeout
- host key changes
- DNS or hostname resolution
- connection refused

The two common cases are:

**Tailscale SSH "additional check required":**

```text
✗ host-1: ssh probe failed: Tailscale SSH requires an additional
  check — open this URL in a browser, then re-run:
    https://login.tailscale.com/a/<token>
```

Open the URL. Approve the device. Then run the command again.

**Permission denied:**

```text
✗ host-x: ssh probe failed: permission denied — check the ssh key
  (`ssh-add -l`) and that the deploy user is configured on the host
```

Add the key with `ssh-add ~/.ssh/id_ed25519` (or the key that you use). Then run the command again.

A different cause is a host on which Docker is not installed yet. This is common on a first boot that `cloud-init` drives. `yoink preflight --wait 90s` polls each host with backoff (2s → 15s capped) until Docker responds or the time budget ends. You do not need your own `until ssh … cloud-init status --wait` loop before the first `yoink up`.

## `<command> exited with status N: <stderr>`

`provider: command` ran your secrets binary and the binary failed. The error includes the captured stderr.

Read the stderr first. Then go through these common causes:

- The CLI needs a session or token environment variable that is not set on this runner (for example `DOPPLER_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`, `VAULT_TOKEN`). Run the same command manually to make sure.
- The CLI is missing on the deploy runner. For example, your laptop has it from Homebrew but CI does not. Install it in the workflow before `yoink up`.
- The configured project, vault, or secret name changed. Compare the argv with the UI of your secret manager.

## `parse secrets bundle from <command> (treated as <format>)`

The command exited 0 but yoink did not parse stdout. One of these is the cause:

- The output was empty. The CLI silently filtered to zero secrets. The usual cause is a permissions error at the source.
- A value starts with `{` and triggers the `auto` JSON heuristic. Set `format: dotenv` (or `format: json`) explicitly in `yoink.yaml`.
- The dotenv stream contains a malformed line (no `=`). yoink does not skip malformed lines. The error names the line number.

## `image_present=false; pull failed: 404 not found`

Your CI did not build and push this SHA yet.

Do one of these:

- Wait until the build workflow finishes.
- Pass a different `--tag <service>=<sha>` that exists.
- Add a `build:` block to the service and pass `--build`. Then yoink ships the local image directly and does not pull.

## `secrets.NAME not allowed in step-level if:` (GitHub Actions)

GitHub Actions does not expand `secrets.*` in `if:` expressions.

Move the gate to the job level. As an alternative, compute a boolean in an earlier step and gate on its output.

## `` service "X" has no `tag:` in config ``

yoink found no tag for the service. yoink uses the first of these sources that gives a tag:

1. `--tag X=<value>`.
2. A bare `--tag <value>`, for every service that `--service` selects.
3. `tag:` in the service config.
4. With `--service` and no `--tag`: the git short SHA. This rule needs a clean working tree and does not apply with `--allow-dirty`.

`--here` uses the git short SHA for every selected service, also over a config `tag:`. It does not look at uncommitted changes.

Set a literal `tag:` in the config, or pass the tag at deploy time.

## `depends_on cycle detected: a → b → a`

`depends_on` must be a DAG.

Break the cycle. Usually one of the edges is wrong. For example, a sidecar must depend on its parent, and not the reverse.

## `service "A".networks references undeclared network "X"`

A service-level `networks: [X]` requires that `X` is in `deploy.networks`.

Add `X` to `deploy.networks`.

## `healthcheck timed out after 60s`

The new container started but `healthcheck_path` did not return 200 within `healthcheck_timeout` (default `60s`). The old containers keep running, so you lose no traffic.

yoink removes every container that the failed attempt created for the service on that host. This includes replicas that already passed. yoink prints the last 30 log lines of the failed container and writes them to the audit log as `DeployFailed`.

Find the cause in this list and apply its fix:

- The app is slow to boot. Increase `healthcheck_timeout`.
- The `healthcheck_path` is wrong. yoink sends HTTP `GET <path>` to the container over the docker network. Some apps expose `/healthz` or `/readyz`, not `/health`. Set the path that the app exposes.
- The app crashes. Read the log lines that yoink printed.

## `hook "X" failed: exit 1`

A `pre_deploy` hook found its image and ran, but exited non-zero. yoink aborts the deploy before it swaps the runtime container, so the old version stays live.

The error includes the stdout and the stderr of the hook. Common causes are a migration that fails, a missing migration secret (a different role from the runtime), and schema drift that the migration did not expect.

## `permission denied: /var/run/docker.sock` on the host

The deploy user is not in the `docker` group on the target host.

1. Add the deploy user to the group:

   ```sh
   ssh root@<host> 'usermod -aG docker deploy'
   ```

2. Log out and log in again (or restart the SSH session). Then the group change takes effect.

## TUI shows containers but `yoink up` says `already at spec — no-op`

This is the intended behavior. The [`spec_hash`](/docs/explanation/architecture#drift-detection) matches, so yoink has nothing to do. No part of the input changed.

To force a redeploy, change a label:

```yaml
labels:
  redeploy_nonce: "2026-04-26"
```

## See also

- [Architecture](/docs/explanation/architecture): drift detection, the deploy lock, and the healthcheck-gated swap.
- [CLI reference](/docs/reference/cli): every flag and subcommand, with the diagnostic commands (`yoink doctor`, `yoink validate`, `yoink status`).
- [Configuration reference](/docs/reference/config): the schema for everything in `yoink.yaml`.
