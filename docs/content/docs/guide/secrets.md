---
title: Secrets
weight: 6
---

Two providers, one mental model: a key→value bundle resolved at deploy time and injected as env vars into the containers that reference it. The default (`provider: age`) seals the bundle into a file you commit to git; the escape hatch (`provider: command`) shells out to whatever secret manager you already run.

Either way, **the resolved values feed into `yoink.spec_hash`** — rotating a secret triggers a redeploy, exactly like a config change.

## Sealed secrets (age) — the default

age uses an **asymmetric keypair**, the same shape SSH and GPG use. One `yoink secrets key generate` prints two things at once — they are *not* interchangeable, and where each one belongs is the whole mental model:

| | What it looks like | Where it lives | What it can do |
|---|---|---|---|
| **Recipient** (public half) | `age1w8jcq22re378p38…` | `secrets.recipients:` in `yoink.yaml`, committed to git | **Seal** new values into `secrets.age`. Cannot decrypt. |
| **Identity** (private half) | `AGE-SECRET-KEY-1QQPQZ…` | Your secret manager (GitHub Actions secret, 1Password, AWS SM, …); surfaces at deploy time as `YOINK_AGE_KEY` | **Unseal** existing values. Also seals. Never commit. |

The asymmetry is what makes this safe to commit: the file in your repo is sealed against the public recipient, so anyone reading the repo (collaborators, leaked clone, mirrored backup) sees ciphertext only. Decrypting requires the private identity, which never lands in git.

A few consequences worth internalizing:

- **Sealing only needs the recipient.** `yoink secrets edit` / `seal` work without `YOINK_AGE_KEY` if you're only *adding* values (you'd then need the identity to view existing ones, since `edit` round-trips through decrypt-edit-reseal). The encrypt-only operator is a coherent role — useful for "junior engineer can submit a new secret via PR; only the on-call can read what's already there."
- **Multiple recipients = multiple readers.** `secrets.recipients:` is a list, not a single value. Each recipient is an independent decryption identity — sealing against `[recipient_a, recipient_b]` produces a single file that *either* identity can decrypt. That's how rotation works (transition state with both old and new recipients), and how multi-operator setups work (each engineer's age key is a recipient).
- **`yoink secrets key public`** re-derives the recipient from whichever identity yoink would use right now — handy "is the key in my shell the same one `yoink.yaml` expects?" sanity check.
- **Recovering a recipient from an identity is trivial; the reverse is impossible.** If you lose the recipient string but still have the identity, `yoink secrets key public` regenerates it. If you lose the identity, every value sealed against it is unrecoverable. Plan backups around the identity (see [Backup and recovery](#backup-and-recovery)).

> **Want to try before touching real config?** The [`examples/sealed-secrets/`](https://github.com/oddur/yoink/tree/main/examples/sealed-secrets) walkthrough exercises every step below end-to-end against your laptop's docker daemon — no ssh, no registry.

## Threat model

What sealed secrets cover:

- **Repo / git history exposure.** Anyone with read access to the repo (collaborators, mirrors, leaked clone tarballs, future Dependabot bots) sees only ciphertext; without a matching age identity they can't recover values.
- **Backup blast radius.** The repo backups, the CI artifact cache, the dropped laptop with a checkout — none of those leak secrets so long as the age identity isn't co-located.

What they do **not** cover:

- **A compromised `YOINK_AGE_KEY`.** The key holder *is* the trust anchor. If a CI runner is breached, an operator's laptop is stolen unlocked, or `YOINK_AGE_KEY` shows up in a pasted-into-Slack workflow log, every value sealed against that recipient is exposed. Plan rotation accordingly (see [Compromised key](#compromised-key--emergency-rotation) below).
- **Runtime exposure on the host.** Once decrypted, values land in container env vars. Anyone with `docker exec` or `docker inspect` against the running daemon can read them. Yoink's host model assumes the docker socket is already privileged — it's not a sandbox boundary.
- **Audit trail of which value got read by whom.** age has none. Sealed values are committed once; from then on every operator with the identity decrypts silently. Reach for `provider: command` against a managed store (Vault, Doppler, AWS SM) when "who looked at this and when" needs to be answerable.
- **Per-key git diffs.** The whole `secrets.age` file changes on every edit, so PR review can't show "which key moved." `provider: command` with [sops](#sops) keeps key names plaintext and only encrypts values for that case.

## One-time setup

If you ran `yoink init`, you can skip this section — `init` already generated an identity, saved it to `~/.config/yoink/keys/<recipient>.key`, added a `secrets:` block to your `yoink.yaml`, and printed a backup notice. Skip ahead to [Sealing values](#sealing-values).

Otherwise, the manual flow is two steps.

{{% steps %}}

### Generate a keypair

```sh
yoink secrets key generate
```

Writes a fresh identity to `~/.config/yoink/keys/<recipient>.key` (mode 0600) and prints the matching **recipient** (the `age1…` public half) for the next step. yoink discovers the key automatically every time you seal or unseal — no env var to set, no per-project gitignore to maintain. Multiple projects with different identities coexist in the dir; the filename is the recipient, so yoink picks the right one for each `yoink.yaml`.

**Back the key up.** It's the only thing that can decrypt your sealed values; lose it and the values in this repo are unrecoverable. Pick at least one:

- Password manager: `cat ~/.config/yoink/keys/<recipient>.key` and paste into 1Password / Bitwarden / Keychain.
- Encrypted backup volume: `cp ~/.config/yoink/keys/<recipient>.key ~/Backups/`.
- Teammate handoff: add their `age1…` recipient to `yoink.yaml` (multi-recipient sealing) — defence in depth so the file survives losing your laptop.

### Add the recipient to `yoink.yaml`

```yaml
secrets:
  provider: age
  recipients:
    - age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45
```

The recipient is the public half — safe to commit. (`yoink secrets key public` re-derives it from your current identity if you didn't save the printed string.)

{{% /steps %}}

That's setup done — [Sealing values](#sealing-values) below creates `secrets.age` and you commit it normally.

### Routing the identity to a managed store

When you need the identity in a place other than your laptop's keys dir — typically a GitHub Actions secret, but also 1Password, AWS Secrets Manager, macOS Keychain, etc. — pass `--print` to send the secret to **stdout** while everything else (header, recipient, instructions) goes to **stderr**:

```sh
# GitHub Actions
yoink secrets key generate --print | gh secret set YOINK_AGE_KEY --repo you/your-repo

# 1Password
yoink secrets key generate --print | op item create --category=password \
  --title='yoink: your-repo' --vault=Engineering password=-

# AWS Secrets Manager
yoink secrets key generate --print | aws secretsmanager create-secret \
  --name yoink/your-repo --secret-string file:///dev/stdin

# macOS Keychain
yoink secrets key generate --print | security add-generic-password \
  -s yoink-your-repo -a $USER -w
```

The pipe captures only the `AGE-SECRET-KEY-1…` line. The recipient prints to your terminal (stderr) — copy that into `yoink.yaml`.

> **Don't run `key generate --print` bare and copy-paste.** Many shells log stdout to scrollback, iTerm shared sessions, or tmux capture-pane history, and a bare `key generate --print` leaves the identity in your terminal. The default (no `--print`) is safe — it writes to disk and never touches stdout. Use `--print` only when piping directly into a store.

`--out PATH` writes to a specific file (mode 0600) instead — useful for keeping a project-local keyfile alongside `yoink.yaml`.

For a complete CI walkthrough — generate identity, add recipient, wire the workflow — see the [AGE secrets in GitHub Actions recipe](/docs/recipes/age-in-github-actions).

## Sealing values

```sh
yoink secrets edit
```

Decrypts the current `secrets.age` (or starts a fresh one if it doesn't exist), opens it in `$EDITOR` as a plain dotenv:

```env
DATABASE_URL=postgres://user:pass@db.internal/app
JWT_SIGNING_KEY=...
GHCR_TOKEN=ghp_...
```

Save and quit. Yoink validates the dotenv, re-seals against the recipients, and writes the result to `secrets.age` atomically. Commit + push.

One-shot alternative (piping in from elsewhere):

```sh
echo "FOO=bar" | yoink secrets seal
yoink secrets seal --in plain.env --out secrets.age
```

For per-key tweaks during incident response, the TUI's secrets pane (`e` from any view) lets you view / add / edit / remove individual keys without leaving the dashboard. Bulk multi-line edits stay on `yoink secrets edit` — the TUI is per-key only.

## Using the values

Reference each key by name from a service:

```yaml
services:
  - name: api
    image: ghcr.io/you/api
    secrets: [DATABASE_URL, JWT_SIGNING_KEY]
    env_from_secrets:
      OTEL_EXPORTER_OTLP_HEADERS: GRAFANA_AUTH_HEADER
```

Yoink resolves them out of the sealed bundle and feeds them as env vars to the runtime container. The values flow into `spec_hash` too — rotating a secret triggers a redeploy, exactly like a config change. (One consequence worth knowing: editing *any* key in `secrets.age` rerolls every service that references *any* secret. The dry-run output (`yoink up --dry-run`) shows the diff before you merge.)

## Multiple environments (staging / prod)

Most projects want different values in staging vs prod, and ideally a leak of the staging key shouldn't unlock prod. The yoink-native shape:

```
yoink.staging.yaml   secrets.staging.age   YOINK_AGE_KEY (in staging CI)
yoink.prod.yaml      secrets.prod.age      YOINK_AGE_KEY (in prod CI)
```

Each yaml carries its own `secrets.recipients:` (one recipient per environment) and `secrets.file:`. The CI workflow that targets staging only ever has access to the staging *identity*; the prod deploy workflow runs with the prod identity. A compromised staging identity can't decrypt prod's sealed file — that file is sealed against a different recipient, which only the prod identity unlocks.

If staging and prod genuinely share values (a 12-factor "same image, different config" deploy that just needs a different `DATABASE_URL`), you can put both recipients on a single sealed file — either identity decrypts, deploys carry the right env's overrides via `--tag` / per-host yaml, and you only manage one `secrets.age`. The trade-off is that compromise of either identity reveals both environments' values.

## Backup and recovery

The sealed file is in git, replicated everywhere the repo is. The age identity is the irreplaceable part: lose it and every value sealed against it is unrecoverable.

- **Keep the identity in two stores that can't fail together.** Primary in your secret manager (GitHub Actions secret, 1Password, AWS SM, Vault); recovery copy offline (sealed envelope in a safe, encrypted USB, second manager under a different account). Don't co-locate them under one SSO — a single compromise wipes both.
- **Test the recovery path quarterly.** Decrypt `secrets.age` on a clean machine using only the recovery key. An untested copy is not a backup.
- **Rotation isn't a backup substitute.** Rotating swaps the active identity; it doesn't help if the current one is already gone. If both copies are lost, re-seal from scratch — recover values from upstream sources (Stripe dashboard, AWS console, the manager's CLI) and accept that anything else is lost.

## Compromised key — emergency rotation

If `YOINK_AGE_KEY` lands somewhere it shouldn't (Slack paste, leaked workflow log, stolen unlocked laptop, terminated employee's password manager), rotate **today** — the rotation flow below is the same one for routine hygiene, but the urgency is different and so is the cleanup.

The compromised key can decrypt every value sealed against it for as long as the operator can pull the repo, so:

{{% steps %}}

### Generate a new identity, re-seal, ship

Run `yoink secrets rotate` (or the manual flow). From this point forward, only the new identity decrypts new sealed-file revisions.

### Rotate every value in the bundle, not just the key

A leaked age key is equivalent to leaking every secret it ever decrypted. The attacker has a copy of `secrets.age` (it's in the public repo or wherever they got the key from) and now decrypts it freely. So: roll the database password, regenerate the JWT signing key, rotate the GHCR token, etc., and re-seal those new values into the new sealed file. Use the post-incident checklist your org normally runs for "secret X leaked" — yoink's rotation only swaps the wrapping key, not the contents.

### Revoke any cached copies of the old key

GitHub Actions secret: delete and replace. 1Password / vault entries: delete the old version, not just edit it (some managers keep history). Laptop key files: `srm` / `shred` (or just remove and let the SSD's GC eventually take it).

### Audit the deploy log

`git log secrets.age` shows when the file was last changed; `gh run list --workflow=deploy.yml` shows who triggered deploys with the key. If an unknown deploy ran in the leak window, treat the host as suspect.

{{% /steps %}}

## Identity resolution

When yoink needs to decrypt, it tries these sources in order and stops at the first match:

| Order | Source | When it's used |
|---|---|---|
| 1 | `YOINK_AGE_KEY` env (raw key) | CI / managed-env contexts |
| 2 | `YOINK_AGE_KEY_FILE` env (path) | Explicit override |
| 3 | `~/.config/yoink/keys/*.key` | Laptop default — yoink scans the dir and picks whichever key's public half matches one of `yoink.yaml`'s `secrets.recipients:` |
| 4 | `~/.config/yoink/age.key` | Legacy single-key path; still loaded for back-compat |

The keys-dir scan is what makes multiple projects work without per-project env-var setup. `yoink secrets key generate` writes there by default, and the right project's key gets picked automatically based on the loaded `yoink.yaml`.

Run `yoink secrets key public` from inside any project for a "what key is yoink using here?" sanity check — it prints the recipient derived from whichever identity resolved.

## Rotating a key

```sh
yoink secrets rotate
```

Generates a new identity, re-seals `secrets.age` against [existing recipients + new public key], and prints the new private + public so you can update CI / your secret manager. The transitional state has both keys able to decrypt — no deploy outage during the swap.

**When is it safe to drop the old recipient?** Concretely: after at least one successful `yoink up` has run with *only* the new private key in `YOINK_AGE_KEY` (no fallback). Verify by checking the deploy log — if the run reads `secrets.age` and reconciles services without an "age decrypt failed" error, every consumer of the sealed file is on the new key. Until that confirmation, leave both recipients in place; a premature drop bricks any operator / CI runner still on the old key (they can no longer decrypt new edits).

Once that's confirmed: remove the old recipient from `yoink.yaml` and run `yoink secrets edit` (save without changes) to drop it from the sealed file. Wipe the old `YOINK_AGE_KEY` from your manager only after the recipient is gone — until then it's still legitimately in use.

Manual flow:

{{% steps %}}

### Generate a new identity

```sh
yoink secrets key generate
```

Writes to `~/.config/yoink/keys/<new-recipient>.key` and prints the recipient.

### Add the new recipient alongside the old one

```yaml
secrets:
  recipients:
    - age1...old
    - age1...new
```

### Re-seal against both

```sh
yoink secrets edit   # save without changes
git commit -am 'rotate age recipient'
```

### Update CI to the new private key

`gh secret set YOINK_AGE_KEY` (or the equivalent for your store) with the new identity. Use `--print` for a clean pipe.

### Run a deploy with only the new key

Confirm it succeeds end-to-end before dropping the old one.

### Drop the old recipient

Remove from `secrets.recipients:`, re-seal one more time, commit, then delete the old key from your manager.

{{% /steps %}}

## When age isn't enough

Switch to `provider: command` (next section) when you need:

- **Per-key git diffs** instead of opaque-blob churn — use [sops](#sops). Same "sealed file in the repo" model, but only values are encrypted, so PR review shows which key moved.
- **Cloud KMS as the root of trust** (AWS / GCP / Azure / Vault transit) so no private key ever lives on operator laptops — sops covers this too.
- **Centralized rotation across many repos** — Doppler, 1Password, the Infisical CLI.
- **An audit trail of who read which value when** — Doppler, Vault, AWS Secrets Manager.
- **Fine-grained ACLs** (per-team, per-environment) — Vault, AWS SM, sops with KMS.
- **Secrets that must NOT live in a git history** (regulatory) — anything that's a managed service.

---

## External secrets via CLI (`provider: command`)

`provider: command` is yoink's bring-your-own-tool secrets path. Yoink invokes the configured command, reads the resulting bundle from stdout, and feeds it to the same machinery that `provider: age` uses. No first-party integrations to maintain, no SDK to vendor — operators wire whatever secret store they already run.

```yaml
secrets:
  provider: command
  command: ["doppler", "secrets", "download", "--no-file", "--format", "env"]
```

Yoink spawns that command once per `yoink up` (and once on TUI startup), reads stdout, and parses it. Format is auto-detected: stdout starting with `{` is parsed as JSON, anything else as dotenv (`KEY=value\n`). Override with `format: dotenv` or `format: json` if a value legitimately starts with `{` and confuses the heuristic.

```yaml
secrets:
  provider: command
  command: [...]
  format: dotenv     # or `json`; `auto` is the default
```

Stderr is captured and surfaced when the command exits non-zero. No shell interpolation — yoink spawns the binary directly with the supplied argv (no `sh -c`). If you need pipes or env-var expansion, wrap the call in a shell script and point yoink at that.

### Constraints worth knowing

- **Stdout must be bundle-shaped.** Anything printed to stdout is parsed as the secrets bundle — no progress bars, no `Logging in…` banners, no `set -x` traces. Print diagnostics to stderr (`>&2`). Yoink caps stdout at 10 MB and the call at 60 s wall-clock.
- **The spawn environment is allowlisted.** Yoink forwards only `PATH`, `HOME`, `USER`, `XDG_CONFIG_HOME`, `LANG`, `LC_ALL`, `TERM`, `TZ`, plus per-tool auth tokens (`DOPPLER_TOKEN`, `INFISICAL_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`, `VAULT_ADDR`/`VAULT_TOKEN`, `AWS_*`, `AZURE_*`, `GOOGLE_APPLICATION_CREDENTIALS`, `BWS_ACCESS_TOKEN`, `SOPS_AGE_KEY`/`SOPS_AGE_KEY_FILE`). `YOINK_AGE_KEY` is **not** forwarded. Other env vars need a wrapper script that re-exports them.
- **All allowlisted tokens flow to any provider you invoke.** If both `SOPS_AGE_KEY` and `DOPPLER_TOKEN` are in the shell, both reach the spawned command regardless of which provider it is. Run yoink in an env-scrubbed wrapper if that's not acceptable.
- **`secrets.file:` rejects `..` and absolute paths but does not follow symlinks.** A committed sealed file that is a symlink: on read, age-decrypt fails on the target's bytes; on write, `rename(2)` replaces the symlink, leaving the original target untouched. Don't commit symlinked sealed files — that's a PR-review concern, not yoink's.

### sops

[sops](https://github.com/getsops/sops) is the most natural fit for teams that want sealed secrets in the repo (like the `provider: age` default) but with the extra capabilities sops layers on top:

- **Per-key git diffs.** sops encrypts only values; key names stay plaintext. PR review of a secret change shows *which key* moved instead of "this opaque blob changed".
- **Multi-cloud KMS as the root of trust.** Encrypt to AWS KMS, GCP KMS, Azure Key Vault, or Vault transit — decrypt via IAM role at deploy time, no private key on operator laptops.
- **Per-value MAC** for tamper detection at the field level.
- **Granular rotation.** `sops updatekeys` re-keys the data key without re-encrypting every value.
- **Format flexibility.** Encrypts YAML / JSON / INI / dotenv / binary; supports nested structures.

```yaml
secrets:
  provider: command
  command: ["sops", "-d", "--output-type=dotenv", "secrets.enc.yaml"]
```

Pre-flight: `brew install sops` on the operator's laptop and CI runner; configure `.sops.yaml` with the encryption recipients (age key, KMS ARN, etc.). Editor flow stays on `sops edit secrets.enc.yaml` (yoink doesn't wrap it).

Trade-off vs the bundled `provider: age`: sops is one extra binary to install, one extra `.sops.yaml` config to maintain, and (with cloud KMS) a network dependency at deploy time. Reach for sops when one of the bullets above is genuinely paying for that overhead — git-diff legibility for many secret reviewers, or KMS as the root of trust for compliance reasons. The `provider: age` default is sized for "small team, single source-of-truth repo, one private key per environment".

> **Cloud-KMS-backed sops makes the deploy network-dependent and IAM-dependent.** A `sops -d` against AWS/GCP/Azure KMS calls out to the cloud control plane on every invocation — if the runner can't reach the KMS endpoint (DNS hiccup, VPC route flap, IAM role expired, KMS key disabled mid-rotation), the deploy fails at the secrets step and yoink can't recover. Build for that: alarm on KMS request failures, keep the IAM role attached to the runner narrow but durable (don't tie it to a session that expires under deploys), and have a break-glass plan (a sealed `secrets.age` mirror, or a static export rotated quarterly) for the case where KMS is the outage. The `provider: age` default has none of these network failure modes — the sealed file is local and `YOINK_AGE_KEY` is just a string.

### Doppler

```yaml
secrets:
  provider: command
  command: ["doppler", "secrets", "download", "--no-file", "--format", "env"]
```

CI: `DOPPLER_TOKEN` env var with a service-token scoped to the right config. No `doppler login` needed on the runner.

### Infisical CLI

```yaml
secrets:
  provider: command
  command:
    - infisical
    - export
    - --env=prod
    - --projectId=YOUR_PROJECT_ID
    - --format=dotenv
```

Auth: `INFISICAL_TOKEN` (machine identity) or a logged-in `infisical login` session. Self-hosted instances: pass `--domain=https://infisical.your.domain` in the argv.

PEM certs and private keys round-trip cleanly — Infisical's dotenv emitter wraps them in single quotes spanning multiple lines, and yoink's dotenv parser handles that shape natively. No wrapper script needed.

### HashiCorp Vault

```yaml
secrets:
  provider: command
  command: ["sh", "-c", "vault kv get -format=json -field=data secret/yoink/prod"]
  format: json
```

Vault's flat KV path → JSON object on stdout. Auth: `VAULT_ADDR` + `VAULT_TOKEN` (or AWS / OIDC roles). The `sh -c` wrapper is needed because we want vault's `-field=data` extracted and re-emitted; if your Vault path already returns a plain `{"K":"V"}` object, drop the wrapper.

### AWS Secrets Manager

```yaml
secrets:
  provider: command
  command:
    - sh
    - -c
    - "aws secretsmanager get-secret-value --secret-id yoink/prod --query SecretString --output text"
  format: auto
```

Store the value as either a JSON object (`{"DB_URL":"...","API_KEY":"..."}`) or a dotenv blob in the SecretString. Auth via the runner's IAM role.

### 1Password

1Password is structured items, not a flat namespace, so the cleanest fit is a small wrapper script that emits dotenv:

```sh
# bin/yoink-secrets.sh — operator-owned, gitignored alongside yoink.yaml
#!/usr/bin/env bash
set -euo pipefail
op read "op://Engineering/yoink-prod/DATABASE_URL"        | sed 's/^/DATABASE_URL=/'
op read "op://Engineering/yoink-prod/JWT_SIGNING_KEY"     | sed 's/^/JWT_SIGNING_KEY=/'
op read "op://Engineering/yoink-prod/STRIPE_SECRET_KEY"   | sed 's/^/STRIPE_SECRET_KEY=/'
```

```yaml
secrets:
  provider: command
  command: ["./bin/yoink-secrets.sh"]
```

Or `op inject` against a template:

```sh
# secrets.tpl
DATABASE_URL={{ op://Engineering/yoink-prod/DATABASE_URL }}
JWT_SIGNING_KEY={{ op://Engineering/yoink-prod/JWT_SIGNING_KEY }}
```

```yaml
secrets:
  provider: command
  command: ["op", "inject", "-i", "secrets.tpl"]
```

CI: `OP_SERVICE_ACCOUNT_TOKEN` env var with a 1Password service account.

### Bitwarden

```yaml
secrets:
  provider: command
  command: ["bws", "secret", "list", "PROJECT_UUID", "--output", "env"]
```

Auth: `BWS_ACCESS_TOKEN`.

### A static dotenv file (for local development)

```yaml
secrets:
  provider: command
  command: ["cat", ".env.local"]
```

Useful when you want the same `provider: command` schema in both prod (managed) and local (a plain file). Add `.env.local` to `.gitignore`.

### Layered: command on top of age

Yoink doesn't natively merge providers, but the wrapper trick gets you there:

```sh
# bin/yoink-secrets.sh
yoink secrets show --reveal              # age-decrypted dotenv (committed)
op inject -i ops-overrides.tpl           # 1Password-managed runtime tweaks
```

```yaml
secrets:
  provider: command
  command: ["./bin/yoink-secrets.sh"]
```

Later lines override earlier ones (the dotenv parser is last-write-wins per key).

## Format reference

**dotenv** (default for stdout that doesn't start with `{`):

```env
# comments and blank lines OK
KEY=value
QUOTED="value with spaces"
SINGLE_QUOTED='value'
export PREFIX_OK=true     # leading `export ` stripped

# Quoted values may span multiple lines — useful for PEM certs and
# private keys, which is what `infisical export --format=dotenv`,
# `doppler secrets download --format env`, and shell `set` emit
# without you having to do anything.
TLS_CERT='-----BEGIN CERTIFICATE-----
MIIErDCCA5SgAwIBAgIUd...
-----END CERTIFICATE-----'
```

A UTF-8 BOM at the start (`\xEF\xBB\xBF`) is silently stripped — Windows tooling sometimes emits one.

**JSON** (stdout starting with `{`):

```json
{"KEY":"value","QUOTED":"value with spaces","NESTED_NULL_DROPPED":null}
```

Top-level must be an object of `string → string` pairs. `null` values are dropped. **Non-string scalars (numbers, booleans) and nested objects/arrays are a hard error** — silently stringifying `8080` to `"8080"` or `true` to `"true"` masks operator typos and produces values consumers don't expect. If you genuinely need a numeric or boolean secret, emit it as a JSON string at the source.

A malformed bundle is a hard error — yoink would rather fail loud than silently drop a typo'd export.

## See also

- [AGE secrets in GitHub Actions](/docs/recipes/age-in-github-actions) — short recipe for the CI-side wiring.
- [Configuration reference: secrets](/docs/reference/config) — full schema for both providers.
