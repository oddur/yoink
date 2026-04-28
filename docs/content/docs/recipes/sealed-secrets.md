---
title: Sealed secrets (age)
weight: 1
---

The default secrets path. One sealed file committed to the repo, one age identity routed through whatever secret manager you already use, decrypted at deploy time with no remote service in the loop.

> **Want to try it before touching your real config?** Clone the repo and run [`examples/sealed-secrets/`](https://github.com/oddur/yoink/tree/main/examples/sealed-secrets) — a self-contained walkthrough against your laptop's docker daemon (no ssh, no registry). It exercises every step below end-to-end with an alpine container.

## Two halves of one key

age uses an **asymmetric keypair**, the same shape SSH and GPG use. One `yoink secrets key generate` prints two things at once — they are *not* interchangeable, and where each one belongs is the whole mental model:

| | What it looks like | Where it lives | What it can do |
|---|---|---|---|
| **Recipient** (public half) | `age1w8jcq22re378p38…` | `secrets.recipients:` in `yoink.yaml`, committed to git | **Seal** new values into `secrets.age`. Cannot decrypt. |
| **Identity** (private half) | `AGE-SECRET-KEY-1QQPQZ…` | Your secret manager (GitHub Actions secret, 1Password, AWS SM, …); surfaces at deploy time as `YOINK_AGE_KEY` | **Unseal** existing values. Also seals. Never commit. |

The asymmetry is what makes this safe to commit: the file in your repo is sealed against the public recipient, so anyone reading the repo (collaborators, leaked clone, mirrored backup) sees ciphertext only. Decrypting requires the private identity, which never lands in git.

A few consequences worth internalizing:

- **Sealing only needs the recipient.** `yoink secrets edit` / `seal` work without `YOINK_AGE_KEY` if you're only *adding* values (you'd then need the identity to view existing ones, since `edit` round-trips through decrypt-edit-reseal). In practice operators always have both, but the encrypt-only operator is a coherent role — useful for "junior engineer can submit a new secret via PR; only the on-call can read what's already there."
- **Multiple recipients = multiple readers.** `secrets.recipients:` is a list, not a single value. Each recipient is an independent decryption identity — sealing against `[recipient_a, recipient_b]` produces a single file that *either* identity can decrypt. That's how rotation works (transition state with both old and new recipients), and how multi-operator setups work (each engineer's age key is a recipient).
- **`yoink secrets key public`** re-derives the recipient from whichever identity yoink would use right now — handy "is the key in my shell the same one `yoink.yaml` expects?" sanity check.
- **Recovering a recipient from an identity is trivial; the reverse is impossible.** If you lose the recipient string but still have the identity, `yoink secrets key public` regenerates it. If you lose the identity, every value sealed against it is unrecoverable. Plan backups around the identity (see [Backup and recovery](#backup-and-recovery)).

## Threat model — what age does and doesn't protect

What it covers:

- **Repo / git history exposure.** Anyone with read access to the repo (collaborators, mirrors, leaked clone tarballs, future Dependabot bots) sees only ciphertext; without a matching age identity they can't recover values.
- **Backup blast radius.** The repo backups, the CI artifact cache, the dropped laptop with a checkout — none of those leak secrets so long as the age identity isn't co-located.

What it does **not** cover:

- **A compromised `YOINK_AGE_KEY`.** The key holder *is* the trust anchor. If a CI runner is breached, an operator's laptop is stolen unlocked, or `YOINK_AGE_KEY` shows up in a pasted-into-Slack workflow log, every value sealed against that recipient is exposed. Plan rotation accordingly (see "Compromised key" below).
- **Runtime exposure on the host.** Once decrypted, values land in container env vars. Anyone with `docker exec` or `docker inspect` against the running daemon can read them. Yoink's host model assumes the docker socket is already privileged — it's not a sandbox boundary.
- **Audit trail of which value got read by whom.** age has none. Sealed values are committed once; from then on every operator with the identity decrypts silently. Reach for `provider: command` against a managed store (Vault, Doppler, AWS SM) when "who looked at this and when" needs to be answerable.
- **Per-key git diffs.** The whole `secrets.age` file changes on every edit, so PR review can't show "which key moved." `provider: command` with [sops](secrets-external-cli#sops) keeps key names plaintext and only encrypts values for that case.

## One-time setup

1. **Generate a keypair.**

   ```sh
   yoink secrets key generate
   ```

   This prints both halves to stdout — the **identity** (private, `AGE-SECRET-KEY-1…`) and the matching **recipient** (public, `age1…`). See [Two halves of one key](#two-halves-of-one-key) above for which goes where.

   Yoink intentionally does **not** save the identity to a global location like `~/.config/yoink/age.key` because multiple projects with distinct identities would collide there. **You** decide where to route the private half — the patterns below pipe it straight into a secret manager so the identity never sits in scrollback.

   > **Avoid the shell-history hazard.** Run `yoink secrets key generate` *piped directly* into your manager (the patterns below). Don't run it bare and copy-paste from the terminal: many shells log stdout to scrollback / iTerm shared sessions / tmux capture-pane history, and `key generate | tee` into a file leaves the identity in the running shell's history. If you must inspect it, prefix the command with a space (zsh `HISTORY_IGNORE_SPACE` / bash `HISTCONTROL=ignorespace`) and `clear` afterwards.

   Common patterns (pick one):

   ```sh
   # GitHub Actions: paste straight into a repo secret
   yoink secrets key generate | gh secret set YOINK_AGE_KEY --repo you/your-repo

   # Local file (gitignored — `*.key` and `age.key` ABSOLUTELY MUST be
   # in .gitignore before this command runs; `--out` writes mode 0600
   # but git happily commits a 0600 file)
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

   These commands route the **identity** (private half) into a manager. The piped pattern only stores the secret half — the recipient gets repeated in step 2, where it ends up in `yoink.yaml`. See `docs/recipes/secrets-*` for the per-tool deploy-time wiring (`gh actions`, `op read`, `aws secretsmanager get-secret-value`, etc.) that surfaces the identity as `YOINK_AGE_KEY` for the `yoink up` step.

2. **Add the recipient (public half) to `yoink.yaml`** — this is the half that's safe to commit. The `key generate` output prints it alongside the identity; `yoink secrets key public` re-derives it from whichever identity yoink would use right now (run this if you only saved the identity and need the recipient back):

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

Yoink resolves them out of the sealed bundle and feeds them as env vars to the runtime container. The values flow into `spec_hash` too — rotating a secret triggers a redeploy, exactly like a config change. (One consequence worth knowing: editing *any* key in `secrets.age` rerolls every service that references *any* secret. The dry-run output (`yoink up --dry-run`) shows the diff before you merge.)

## CI

```yaml
# .github/workflows/deploy.yml
- run: yoink up --tag api=${{ github.sha }}
  env:
    YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
```

That's the whole CI integration — yoink finds the key in the env var, decrypts on the runner, deploys.

> **Forks and Dependabot.** GitHub doesn't expose `secrets.*` to workflows triggered from forked-repo PRs by default — a `pull_request` from `someone-elses-fork` won't have `YOINK_AGE_KEY` and your dry-run/deploy step will fail with "no age identity found." That's the safe default. For internal PRs from branches in the same repo, secrets are exposed normally. If you genuinely need fork-CI to read secrets, use `pull_request_target` carefully (review the workflow YAML at `head_ref`, not at the fork's PR) — the GitHub Actions docs cover the trade-offs. Dependabot PRs are a similar case: they run with their own restricted secret scope (`secrets.dependabot.*`) — set `YOINK_AGE_KEY` there too if you want Dependabot to dry-run secret-touching changes.

## Multiple environments (staging / prod)

Most projects want different values in staging vs prod, and ideally a leak of the staging key shouldn't unlock prod. The yoink-native shape:

```
yoink.staging.yaml   secrets.staging.age   YOINK_AGE_KEY (in staging CI)
yoink.prod.yaml      secrets.prod.age      YOINK_AGE_KEY (in prod CI)
```

Each yaml carries its own `secrets.recipients:` (one recipient per environment) and `secrets.file:`. The CI workflow that targets staging only ever has access to the staging *identity*; the prod deploy workflow runs with the prod identity. A compromised staging identity can't decrypt prod's sealed file — that file is sealed against a different recipient, which only the prod identity unlocks.

If staging and prod genuinely share values (a 12-factor "same image, different config" deploy that just needs a different `DATABASE_URL`), you can put both recipients on a single sealed file — either identity decrypts, deploys carry the right env's overrides via `--tag` / per-host yaml, and you only manage one `secrets.age`. The trade-off is that compromise of either identity reveals both environments' values.

## Backup and recovery

The sealed file is in git, so it's already replicated everywhere your repo is. The age identity is the irreplaceable part — losing it makes every value sealed against it permanently unrecoverable.

What that means in practice:

- **The identity must live in two places.** Whatever store holds the canonical key (GitHub Actions secret, 1Password vault, AWS SM, Vault, …) is the primary; pick a second offline copy as the recovery key — common patterns are a sealed envelope in a safe, an encrypted USB stick in a desk drawer, or a second password manager owned by a different operator. Don't co-locate them (two GitHub repos in the same org, two 1Password vaults under the same SSO) — a single account compromise wipes both copies.
- **Test the recovery path before you need it.** Once a quarter, decrypt `secrets.age` on a clean machine using *only* the recovery key. If you can't, you don't have a backup — you have a copy you've never verified.
- **Rotation isn't a backup substitute.** Rotating the key (next section) replaces the active identity but doesn't help if you lost the *current* one. If both copies of the live key are gone, you re-seal from scratch — every value the team can still recover from upstream sources (Stripe dashboard, AWS console, the manager's CLI), and accept that anything else is lost.

## Compromised key — emergency rotation

If `YOINK_AGE_KEY` lands somewhere it shouldn't (Slack paste, leaked workflow log, stolen unlocked laptop, terminated employee's password manager), rotate **today** — the rotation flow below is the same one for routine hygiene, but the urgency is different and so is the cleanup.

The compromised key can decrypt every value sealed against it for as long as the operator can pull the repo, so:

1. **Generate a new identity, re-seal, ship.** Run `yoink secrets rotate` (or the manual flow) and merge. From this point forward, only the new identity decrypts new sealed-file revisions.
2. **Rotate every value in the bundle, not just the key.** A leaked age key is equivalent to leaking every secret it ever decrypted. The attacker has a copy of `secrets.age` (it's in the public repo or wherever they got the key from) and now decrypts it freely. So: roll the database password, regenerate the JWT signing key, rotate the GHCR token, etc., and re-seal those new values into the new sealed file. Use the post-incident checklist your org normally runs for "secret X leaked" — yoink's rotation only swaps the wrapping key, not the contents.
3. **Revoke any cached copies of the old key.** GitHub Actions secret: delete and replace. 1Password / vault entries: delete the old version, not just edit it (some managers keep history). Laptop key files: `srm` / `shred` (or just remove and let the SSD's GC eventually take it).
4. **Audit the deploy log.** `git log secrets.age` shows when the file was last changed; `gh run list --workflow=deploy.yml` shows who triggered deploys with the key. If an unknown deploy ran in the leak window, treat the host as suspect.

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

**When is it safe to drop the old recipient?** Concretely: after at least one successful `yoink up` has run with *only* the new private key in `YOINK_AGE_KEY` (no fallback). Verify by checking the deploy log — if the run reads `secrets.age` and reconciles services without an "age decrypt failed" error, every consumer of the sealed file is on the new key. Until that confirmation, leave both recipients in place; a premature drop bricks any operator / CI runner still on the old key (they can no longer decrypt new edits).

Once that's confirmed: remove the old recipient from `yoink.yaml` and run `yoink secrets edit` (save without changes) to drop it from the sealed file. Wipe the old `YOINK_AGE_KEY` from your manager only after the recipient is gone — until then it's still legitimately in use.

Manual flow:

1. `yoink secrets key generate --out new.key`
2. Add the new public key to `secrets.recipients:` *alongside* the old one.
3. `yoink secrets edit` (or `seal`) — re-encrypts to both recipients. Commit.
4. Update `YOINK_AGE_KEY` in your secret manager to the new private key.
5. Run a deploy with only the new key set. Confirm it succeeds end-to-end.
6. Remove the old recipient from `secrets.recipients:`, re-seal one more time, commit, then delete the old key from your manager.

## When age isn't enough

Switch to `provider: command` and let yoink shell out to your secret manager's CLI when you need:

- **Per-key git diffs** instead of opaque-blob churn — use [sops](/docs/recipes/secrets-external-cli#sops). Same "sealed file in the repo" model, but only values are encrypted, so PR review shows which key moved.
- **Cloud KMS as the root of trust** (AWS / GCP / Azure / Vault transit) so no private key ever lives on operator laptops — sops covers this too.
- **Centralized rotation across many repos** — Doppler, 1Password, the Infisical CLI.
- **An audit trail of who read which value when** — Doppler, Vault, AWS Secrets Manager.
- **Fine-grained ACLs** (per-team, per-environment) — Vault, AWS SM, sops with KMS.
- **Secrets that must NOT live in a git history** (regulatory) — anything that's a managed service.

The CLI-driven path covers sops, Doppler, 1Password, HashiCorp Vault, AWS Secrets Manager, the Infisical CLI, Bitwarden, and anything else that emits dotenv or JSON on stdout. See [external secrets via CLI](/docs/recipes/secrets-external-cli).
