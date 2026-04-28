---
title: External secrets via CLI
weight: 2
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

## Per-tool recipes

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
yoink secrets show --reveal --no-mask    # age-decrypted dotenv (committed)
op inject -i ops-overrides.tpl           # 1Password-managed runtime tweaks
```

```yaml
secrets:
  provider: command
  command: ["./bin/yoink-secrets.sh"]
```

Later lines override earlier ones (the dotenv parser is last-write-wins per key).

## Where yoink's own age key goes

The `yoink secrets key generate` private key is itself a secret. Route it through whatever store you trust:

```sh
# Generate, route into your manager
yoink secrets key generate | gh secret set YOINK_AGE_KEY
yoink secrets key generate | op item create --category=password --title='yoink: prod' --vault=Engineering password=-
yoink secrets key generate | aws secretsmanager create-secret --name yoink/prod --secret-string file:///dev/stdin
yoink secrets key generate | vault kv put secret/yoink/prod private=-
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
```

**JSON** (stdout starting with `{`):

```json
{"KEY":"value","QUOTED":"value with spaces","NUMBER":42}
```

Top-level must be an object. Numbers and booleans are stringified; `null` values are dropped.

A malformed bundle is a hard error — yoink would rather fail loud than silently drop a typo'd export.
