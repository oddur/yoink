---
title: Sealed secrets workflow
weight: 14
---

The operator-facing tasks for working with `provider: age` sealed secrets — generating an identity, sealing values, consuming them from services, splitting staging/prod, backups, and rotation. For the mental model (why age, the asymmetric keypair, the threat model), see the [Secrets guide](/docs/guide/secrets).

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

## Routing the identity to a managed store

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

{{< callout type="warning" >}}
**Don't run `key generate --print` bare and copy-paste.** Many shells log stdout to scrollback, iTerm shared sessions, or tmux capture-pane history, and a bare `key generate --print` leaves the identity in your terminal. The default (no `--print`) is safe — it writes to disk and never touches stdout. Use `--print` only when piping directly into a store.
{{< /callout >}}

`--out PATH` writes to a specific file (mode 0600) instead — useful for keeping a project-local keyfile alongside `yoink.yaml`.

For a complete CI walkthrough — generate identity, add recipient, wire the workflow — see the [AGE secrets in GitHub Actions recipe](/docs/how-to/age-in-github-actions).

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
yoink secrets seal --as DEPLOY_KEY=@/tmp/deploy_key   # single key from a file
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

Yoink resolves them out of the sealed bundle and feeds them as env vars to the runtime container. The values flow into [`spec_hash`](/docs/guide/architecture#drift-detection) too — rotating a secret triggers a redeploy, exactly like a config change. (One consequence worth knowing: editing *any* key in `secrets.age` rerolls every service that references *any* secret. The dry-run output (`yoink up --dry-run`) shows the diff before you merge.)

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

## Planned key rotation

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

## Compromised key — emergency rotation

If `YOINK_AGE_KEY` lands somewhere it shouldn't (Slack paste, leaked workflow log, stolen unlocked laptop, terminated employee's password manager), rotate **today** — the rotation flow above is the same one for routine hygiene, but the urgency is different and so is the cleanup.

The compromised key can decrypt every value sealed against it for as long as the operator can pull the repo, so:

{{% steps %}}

### Generate a new identity, re-seal, ship

Run `yoink secrets rotate` (or the manual flow above). From this point forward, only the new identity decrypts new sealed-file revisions.

### Rotate every value in the bundle, not just the key

A leaked age key is equivalent to leaking every secret it ever decrypted. The attacker has a copy of `secrets.age` (it's in the public repo or wherever they got the key from) and now decrypts it freely. So: roll the database password, regenerate the JWT signing key, rotate the GHCR token, etc., and re-seal those new values into the new sealed file. Use the post-incident checklist your org normally runs for "secret X leaked" — yoink's rotation only swaps the wrapping key, not the contents.

### Revoke any cached copies of the old key

GitHub Actions secret: delete and replace. 1Password / vault entries: delete the old version, not just edit it (some managers keep history). Laptop key files: `srm` / `shred` (or just remove and let the SSD's GC eventually take it).

### Audit the deploy log

`git log secrets.age` shows when the file was last changed; `gh run list --workflow=deploy.yml` shows who triggered deploys with the key. If an unknown deploy ran in the leak window, treat the host as suspect.

{{% /steps %}}

## See also

- [Secrets guide](/docs/guide/secrets) — the mental model: asymmetric keypair, threat model, identity resolution, when to switch to `provider: command`.
- [AGE secrets in GitHub Actions](/docs/how-to/age-in-github-actions) — full CI walkthrough.
- [Multi-host Let's Encrypt with Redis](/docs/how-to/multi-host-redis-storage) — separate concern, often paired with sealed secrets.
