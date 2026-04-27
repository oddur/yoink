---
title: Sealed secrets (age)
weight: 1
---

The default secrets path. One sealed file committed to the repo, one key per environment, decrypted at deploy time with no remote service in the loop.

## One-time setup

1. **Generate a key.** Run on your laptop:

   ```sh
   yoink secrets keygen
   ```

   This writes `~/.config/yoink/age.key` (your private identity, *do not commit*) and prints the public recipient (`age1...`).

2. **Add the recipient to `yoink.yaml`:**

   ```yaml
   secrets:
     provider: age
     recipients:
       - age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45
   ```

3. **Add the secret key as a GitHub secret** named `YOINK_AGE_KEY`. Paste the contents of `AGE-SECRET-KEY-1...` (just the key line, not the comment header).

   **Recommended**: instead of pasting your laptop key, generate a separate CI-only identity that doesn't touch disk:

   ```sh
   yoink secrets keygen --ci
   ```

   This prints a new secret + public key without saving anything locally. Paste the secret into the GitHub secret, add the public key to `secrets.recipients:`. See [AGE secrets in GitHub Actions](/docs/recipes/age-in-github-actions) for the full workflow.

4. **Add `secrets.age` to your repo and commit it.** The next step creates it.

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

One-shot alternative (e.g. piping in from somewhere):

```sh
echo "FOO=bar" | yoink secrets seal
yoink secrets seal --in plain.env --out secrets.age
```

## Inspecting

```sh
yoink secrets show              # masked values, just the keys are visible
yoink secrets show --reveal     # full values (for debugging)
```

`show` is read-only — never re-encrypts.

## Using the values

Same as Infisical. Reference each key by name from a service:

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

That's the whole CI integration — yoink finds the key in the env var, decrypts on the runner, and goes.

## Key resolution priority

When yoink needs to decrypt, it looks in this order:

1. `YOINK_AGE_KEY` env var — raw key (used in CI)
2. `YOINK_AGE_KEY_FILE` env var — path to a key file
3. `~/.config/yoink/age.key` — the laptop default

Stop at the first one set. Override at any layer.

## Rotating a key

```sh
yoink secrets rotate
```

Generates a new identity, re-seals `secrets.age` against [existing recipients + new public key], and prints the new secret + public for pasting into a GitHub Actions secret. The transitional state has both keys able to decrypt — no deploy outage during the swap.

After CI is decrypting fine with the new key, remove the old recipient from `yoink.yaml` and run `yoink secrets edit` (save without changes) to drop it from the sealed file.

If you want to do it manually:

1. `yoink secrets keygen --out new.key` — generate a new identity.
2. Add the new public key to `secrets.recipients:` *alongside* the old one.
3. `yoink secrets edit` (or `seal`) — re-encrypts to both recipients. Commit.
4. Update `YOINK_AGE_KEY` in GitHub secrets to the new private key.
5. Once your laptop + CI are both on the new key, remove the old recipient from `secrets.recipients:` and re-seal one more time.

## Trade-offs vs Infisical

| | age (sealed) | Infisical |
|---|---|---|
| Where secrets live | committed in repo | remote service |
| Rotation | manual edit + re-commit | UI / API |
| Audit log of who changed what | git blame | Infisical audit log |
| Network dependency at deploy | none | reachability to Infisical |
| Setup | `keygen` + recipient | machine identity, project, env |
| PR diff signal | opaque blob | n/a (out of repo) |
| Per-environment isolation | one key per env | environments are first-class |

Use age when "just commit the secrets" is the right answer (small team, single environment, or environments split across separate repos). Switch to [Infisical](/docs/recipes/infisical-auth) once you need an audit trail, fine-grained access control, or shared rotation across many repos.
