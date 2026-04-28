---
title: AGE secrets in GitHub Actions
weight: 2
---

How to use age-sealed secrets from a CI workflow. The whole integration is one env var.

> **One-paragraph refresher.** age uses an asymmetric keypair. The **recipient** (`age1…`, public) goes in `yoink.yaml` and seals new values. The **identity** (`AGE-SECRET-KEY-1…`, private) goes in your secret manager and unseals at deploy time. CI gets its own pair: a fresh identity it owns and never shares, plus the matching recipient added alongside the laptop recipient(s) in `yoink.yaml` so either can decrypt. See [Two halves of one key](/docs/recipes/sealed-secrets#two-halves-of-one-key) for the full mental model.

## Prerequisites

You've already set up sealed secrets locally — see [Sealed secrets (age)](/docs/recipes/sealed-secrets) if not. Specifically:

- `secrets.age` is committed to the repo (encrypted)
- `yoink.yaml` has a `secrets.recipients:` list with at least your laptop's recipient
- Your laptop's identity is saved somewhere readable to you (typically a gitignored `age.key` next to the project's `yoink.yaml`)

## 1. Generate a CI-only identity

Each principal who needs to decrypt — your laptop, CI, a teammate's laptop — gets its own age keypair. **Don't reuse the laptop pair for CI**; if a CI runner is compromised you want to revoke its identity without disturbing operators' day-to-day workflow.

Generate a fresh one:

```sh
yoink secrets key generate
```

Default `key generate` prints the secret straight to stdout (it does **not** save to disk by default), which is exactly what you want for CI: paste the printed secret into a GitHub Actions secret and clear your scrollback. Output looks like:

```
New age identity. Save the secret somewhere — yoink won't.

Secret (private — never commit; gitignore the file you save it to):

AGE-SECRET-KEY-1KLY239F...

Public recipient (add to yoink.yaml):

  secrets:
    provider: age
    recipients:
      - age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45

Suggested next steps:
  • Save the secret to ./age.key (gitignored), then:
      export YOINK_AGE_KEY_FILE=$(pwd)/age.key
  • Or paste it into a CI secret named YOINK_AGE_KEY.
  • Clear your terminal scrollback when done.
```

Don't save the **identity** to disk — paste it directly into the GitHub secret. The scrollback is the only copy. The **recipient** is fine to copy around freely; we add it to `yoink.yaml` next.

## 2. Add the CI recipient to `yoink.yaml`

`recipients:` is a list. Add the new CI recipient *alongside* the existing laptop recipient — don't replace it, or you'll lock yourself out of the file you just sealed. Both identities can decrypt the same `secrets.age`; that's the whole reason recipients is a list.

```yaml
secrets:
  provider: age
  recipients:
    - age1...your-laptop-recipient                                    # already there
    - age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45  # CI
```

Re-seal so both identities can decrypt:

```sh
yoink secrets edit         # save without changes — picks up the new recipient
git add yoink.yaml secrets.age
git commit -m "chore: add CI to age recipients"
git push
```

## 3. Paste the secret into GitHub

Repo Settings → Secrets and variables → Actions → New repository secret:

- **Name**: `YOINK_AGE_KEY`
- **Value**: `AGE-SECRET-KEY-1KLY239F...` (the secret from step 1)

Clear your terminal scrollback (`reset` or close the tab).

## 4. Wire it into the workflow

```yaml
# .github/workflows/deploy.yml
name: Deploy
on:
  push:
    branches: [live]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install yoink
        run: |
          curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh
          echo "$HOME/.cargo/bin" >> $GITHUB_PATH

      - name: Deploy
        run: yoink up --tag api=${{ github.sha }} --tag web=${{ github.sha }}
        env:
          YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
```

That's the whole integration. Yoink finds `YOINK_AGE_KEY` in the env, decrypts `secrets.age` on the runner, and uses the values exactly like a laptop deploy would.

## How yoink finds the key

Resolution order (same code path locally and in CI):

1. `YOINK_AGE_KEY` env var — raw key (the CI path)
2. `YOINK_AGE_KEY_FILE` env var — path to a key file (the laptop path: `export YOINK_AGE_KEY_FILE=$(pwd)/age.key`)
3. `~/.config/yoink/age.key` — fallback if you've manually placed a key there. Yoink doesn't write to this path itself.

Stop at the first one set. If none are set, yoink errors with a pointer to `yoink secrets key generate`.

## Rotating the CI key

When you want to roll the CI identity (left the company, suspected leak, periodic hygiene):

```sh
yoink secrets rotate
```

This generates a new identity, re-seals `secrets.age` against [old recipients + new recipient], prints the new secret + public key, and tells you what to do next:

1. Add the new recipient to `yoink.yaml`
2. Update the `YOINK_AGE_KEY` GitHub secret to the new value
3. Once CI is decrypting fine with the new key, remove the old recipient from `yoink.yaml` and run `yoink secrets edit` (save without changes) to drop it.

The transitional state where both keys can decrypt prevents a deploy outage during the swap.

## Checking what's sealed (without revealing values)

In CI debug, useful to confirm yoink can read what you expect:

```yaml
- name: List sealed keys
  run: yoink secrets show       # masked by default
  env:
    YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
```

`show --reveal` prints actual values. Don't use `--reveal` in CI — workflow logs are visible to anyone with read access to the repo. (Yoink also refuses to do this when `$CI` is set; override with `YOINK_ALLOW_REVEAL_IN_CI=1` if you really mean it, e.g. when piping into a different secret store rather than into build logs.)

## Two axes of rotation

Recipients (who can decrypt) and values (what's stored) are independent — an important distinction during incident response. Editing `yoink.yaml` `recipients:` rotates *who can decrypt*; editing `secrets.age` via `yoink secrets edit` rotates *what's stored*. A leaked CI identity needs both: revoke the recipient (so the leaked identity can't decrypt new edits) **and** rotate every value (because the attacker has a copy of the old `secrets.age` and the leaked identity already decrypts it).

## Failure modes

- **`age decryption failed: no matching key`** — CI's secret doesn't match any recipient in `secrets.age`. Either the GitHub secret wasn't updated after a rotation, or you forgot to re-seal after adding the new recipient. Run `yoink secrets show` locally with the same key to confirm.
- **`no age identity found`** — the workflow forgot the `env: YOINK_AGE_KEY:` block on the deploy step.
- **`workflow logs leak the values`** — never use `yoink secrets show --reveal` in CI; never `echo $DATABASE_URL` in a step. GitHub auto-masks values that match registered secrets, but yoink-decrypted plaintext isn't registered.
