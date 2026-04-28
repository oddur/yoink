---
title: External secrets via CLI
weight: 6
---

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

> **Stdout must be bundle-shaped, full stop.** Anything the command prints to stdout is parsed as your secrets bundle — no progress bars, no `Logging in…` banners, no shell `set -x` traces. Wrapper scripts that emit anything other than the dotenv/JSON payload will either fail to parse or (worse) silently treat the noise as a malformed secret. Print human-readable diagnostics to **stderr only** (`>&2`). Yoink also enforces a 10 MB cap on stdout and a 60 s wall-clock timeout — chunk large bundles, and don't shell out to a CLI that retries forever.
>
> **Spawned commands run with a locked-down environment.** Yoink calls `env_clear()` and forwards only an explicit allowlist (`PATH`, `HOME`, `USER`, `XDG_CONFIG_HOME`, `LANG`, `LC_ALL`, `TERM`, `TZ`, plus per-tool auth tokens: `DOPPLER_TOKEN`, `INFISICAL_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`, `VAULT_ADDR`/`VAULT_TOKEN`, `AWS_*`, `AZURE_*`, `GOOGLE_APPLICATION_CREDENTIALS`, `BWS_ACCESS_TOKEN`, `SOPS_AGE_KEY`/`SOPS_AGE_KEY_FILE`). `YOINK_AGE_KEY` is **not** forwarded — yoink's own identity stays out of the third-party CLI's env. If your provider needs another env var, wrap the call in a shell script that re-exports it from a file the script reads itself.
>
> **All allowlisted tokens are a single trust unit.** If you set both `SOPS_AGE_KEY` and `DOPPLER_TOKEN` in the shell that runs `yoink up`, both flow into whichever provider command runs — even if your `provider: command` is `doppler` and has no business seeing the sops key. The threat is bounded (a malicious provider can already see every secret it returns), but treat the env vars listed above as collectively visible to any provider you invoke. If that's not acceptable, run yoink in a wrapper that scrubs the env down to the one tool's tokens before calling.
>
> **`secrets.file:` rejects `..` and absolute paths but does not follow symlinks.** A committed sealed file that is a symlink to somewhere outside the repo will: on read, attempt to age-decrypt the target (fails noisily for non-age files); on write (`yoink secrets edit`/`seal`/`rotate`), be replaced by the new sealed file via `rename(2)`, leaving the original target untouched. Still — don't commit symlinked sealed files; PR review is the right place to catch that, not yoink.

## Per-tool recipes

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

## Where yoink's own age key goes

The `yoink secrets key generate` private key is itself a secret. By default it's saved to `~/.config/yoink/keys/<recipient>.key` and yoink discovers it automatically. For CI / external stores, pass `--print` so the secret goes to stdout (the recipient and follow-up notes go to stderr, so the pipe stays clean):

```sh
# Generate, route into your manager
yoink secrets key generate --print | gh secret set YOINK_AGE_KEY
yoink secrets key generate --print | op item create --category=password --title='yoink: prod' --vault=Engineering password=-
yoink secrets key generate --print | aws secretsmanager create-secret --name yoink/prod --secret-string file:///dev/stdin
yoink secrets key generate --print | vault kv put secret/yoink/prod private=-
```

Then surface it as `YOINK_AGE_KEY` at deploy time:

```sh
# GitHub Actions: inject from repo secret
- run: yoink up
  env:
    YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}

# 1Password CLI on a self-hosted runner
- run: |
    export YOINK_AGE_KEY=$(op read 'op://Engineering/yoink-prod/private')
    yoink up

# AWS instance role
- run: |
    export YOINK_AGE_KEY=$(aws secretsmanager get-secret-value --secret-id yoink/prod --query SecretString --output text)
    yoink up
```

The age key unlocks the sealed `secrets.age` bundle; everything else flows from there.

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

- [Sealed secrets (age)](/docs/recipes/sealed-secrets) — yoink's batteries-included default; `provider: command` is the escape hatch when this isn't enough.
- [AGE secrets in GitHub Actions](/docs/recipes/age-in-github-actions) — CI plumbing if your `provider:` is age.
- [Configuration reference: secrets](/docs/reference/config) — full schema for `provider: command`.
