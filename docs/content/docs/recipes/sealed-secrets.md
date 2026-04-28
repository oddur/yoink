---
title: Sealed secrets (age)
weight: 1
---

The default secrets path. One sealed file committed to the repo, one age identity routed through whatever secret manager you already use, decrypted at deploy time with no remote service in the loop.

> **Want to try it before touching your real config?** Clone the repo and run [`examples/sealed-secrets/`](https://github.com/oddur/yoink/tree/main/examples/sealed-secrets) — a self-contained walkthrough against your laptop's docker daemon (no ssh, no registry). It exercises every step below end-to-end with an alpine container.

## One-time setup

1. **Generate an identity.**

   ```sh
   yoink secrets key generate
   ```

   The private key is printed to stdout — yoink intentionally does **not** save it to a global location like `~/.config/yoink/age.key` because multiple projects with distinct identities would collide there. **You** decide where to route the private half.

   Common patterns (pick one):

   ```sh
   # GitHub Actions: paste straight into a repo secret
   yoink secrets key generate | gh secret set YOINK_AGE_KEY --repo you/your-repo

   # Local file (gitignored)
   yoink secrets key generate --out age.key
   echo age.key >> .gitignore
   export YOINK_AGE_KEY_FILE=$(pwd)/age.key

   # 1Password
   yoink secrets key generate | op item create --category=password \
     --title='yoink: your-repo' --vault=Engineering password=-

   # AWS Secrets Manager
   yoink secrets key generate | aws secretsmanager create-secret \
     --name yoink/your-repo --secret-string file:///dev/stdin

   # macOS Keychain
   yoink secrets key generate | security add-generic-password \
     -s yoink-your-repo -a $USER -w
   ```

   See `docs/recipes/secrets-*` for the per-tool deploy-time wiring (`gh actions`, `op read`, `aws secretsmanager get-secret-value`, etc.) that surfaces the value as `YOINK_AGE_KEY` for the `yoink up` step.

2. **Add the public recipient to `yoink.yaml`** (the `key generate` output prints it; `yoink secrets key public` derives it from your current identity at any time):

   ```yaml
   secrets:
     provider: age
     recipients:
       - age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45
   ```

3. **Add `secrets.age` to the repo and commit it.** The next step creates it.

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

Yoink resolves them out of the sealed bundle and feeds them as env vars to the runtime container. The values flow into `spec_hash` too — rotating a secret triggers a redeploy, exactly like a config change.

## CI

```yaml
# .github/workflows/deploy.yml
- run: yoink up --tag api=${{ github.sha }}
  env:
    YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
```

That's the whole CI integration — yoink finds the key in the env var, decrypts on the runner, deploys.

## Identity resolution

When yoink needs to decrypt, it looks in this order, stopping at the first one set:

1. `YOINK_AGE_KEY` env var — raw key (used in CI / managed-env contexts)
2. `YOINK_AGE_KEY_FILE` env var — path to a key file
3. `~/.config/yoink/age.key` — fallback. Yoink doesn't write to this path itself (multi-project collisions); operators who *want* one global identity put their key there manually.

`yoink secrets key public` prints the recipient (`age1…`) derived from the resolved identity — quick "is the key in my shell the same one yoink.yaml expects?" sanity check.

## Rotating a key

```sh
yoink secrets rotate
```

Generates a new identity, re-seals `secrets.age` against [existing recipients + new public key], and prints the new private + public so you can update CI / your secret manager. The transitional state has both keys able to decrypt — no deploy outage during the swap.

After CI is decrypting fine with the new key, remove the old recipient from `yoink.yaml` and run `yoink secrets edit` (save without changes) to drop it from the sealed file.

Manual flow:

1. `yoink secrets key generate --out new.key`
2. Add the new public key to `secrets.recipients:` *alongside* the old one.
3. `yoink secrets edit` (or `seal`) — re-encrypts to both recipients. Commit.
4. Update `YOINK_AGE_KEY` in your secret manager to the new private key.
5. Once your laptop + CI are both on the new key, remove the old recipient from `secrets.recipients:` and re-seal one more time.

## When age isn't enough

Switch to `provider: command` and let yoink shell out to your secret manager's CLI when you need:

- **Per-key git diffs** instead of opaque-blob churn — use [sops](/docs/recipes/secrets-external-cli#sops). Same "sealed file in the repo" model, but only values are encrypted, so PR review shows which key moved.
- **Cloud KMS as the root of trust** (AWS / GCP / Azure / Vault transit) so no private key ever lives on operator laptops — sops covers this too.
- **Centralized rotation across many repos** — Doppler, 1Password, the Infisical CLI.
- **An audit trail of who read which value when** — Doppler, Vault, AWS Secrets Manager.
- **Fine-grained ACLs** (per-team, per-environment) — Vault, AWS SM, sops with KMS.
- **Secrets that must NOT live in a git history** (regulatory) — anything that's a managed service.

The CLI-driven path covers sops, Doppler, 1Password, HashiCorp Vault, AWS Secrets Manager, the Infisical CLI, Bitwarden, and anything else that emits dotenv or JSON on stdout. See [external secrets via CLI](/docs/recipes/secrets-external-cli).
