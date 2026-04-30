---
title: Pre-merge dry-run on every PR
weight: 7
---

`yoink up --dry-run --format=markdown` connects to each host, computes the diff between the running spec and what would deploy, and emits a markdown summary. Pipe that into a sticky GitHub PR comment and reviewers see exactly what the deploy will change before merging.

## What the comment looks like

> ### yoink dry-run
>
> **Plan:** 0 to create · 2 to update · 4 unchanged · 1 orphan
>
> #### `prod-host-1`
>
> | | service | spec | image |
> |---|---|---|---|
> | ⚪ | `otel` | `d3eb14f` (unchanged) | _(unchanged)_ |
> | ⚪ | `redis` | `dcef8ac` (unchanged) | _(unchanged)_ |
> | 🟡 | `api` | `0ba920d` → `4d8e123` | `ghcr.io/you/api:abc1234` → `ghcr.io/you/api:def5678` |
> | 🟡 | `web` | `67a2fa0` → `a8b3c91` | `ghcr.io/you/web:abc1234` → `ghcr.io/you/web:def5678` |
> | ⚪ | `caddy` | `8edf75f` (unchanged) | _(unchanged)_ |
>
> <details><summary>1 orphan container</summary>
>
> - `prod-host-1@api-old-version` (service: `api`)
>
> </details>

Reviewers can see at a glance: which services this PR rolls, which stay untouched, and whether the `image:` change matches their expectation.

## The workflow

`.github/workflows/yoink-pr-diff.yml`:

```yaml
name: "Yoink: PR dry-run"

on:
  pull_request:
    branches: [main]
    paths:
      - "services/**"
      - "yoink.yaml"
      - ".github/workflows/yoink-pr-diff.yml"

concurrency:
  group: yoink-pr-diff-${{ github.event.pull_request.number }}
  cancel-in-progress: true

permissions:
  contents: read
  pull-requests: write   # marocchino/sticky-pull-request-comment

jobs:
  dry-run:
    runs-on: ubuntu-latest   # or your tailnet-connected runner
    steps:
      - uses: actions/checkout@v4

      # Bring whatever your auth shape is. Tailscale + age-sealed
      # secrets here; swap in your `provider: command` setup if you
      # use a managed store instead.
      - uses: tailscale/github-action@v4
        with:
          oauth-client-id: ${{ secrets.TS_OAUTH_CLIENT_ID }}
          oauth-secret: ${{ secrets.TS_OAUTH_SECRET }}
          tags: tag:ci

      - name: Install yoink
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          YOINK_VERSION: v0.7.0
        run: |
          gh release download "$YOINK_VERSION" --repo oddur/yoink \
            --pattern 'yoink-x86_64-unknown-linux-gnu.tar.xz' --output - \
          | tar -xJ --strip-components=1 -C /usr/local/bin yoink-x86_64-unknown-linux-gnu/yoink

      - name: Compute diff
        env:
          PR_SHA: ${{ github.event.pull_request.head.sha }}
          YOINK_AGE_KEY: ${{ secrets.YOINK_AGE_KEY }}
        run: |
          yoink --config yoink.yaml up --dry-run --format=markdown \
            --tag api=$PR_SHA --tag web=$PR_SHA > diff.md
          cat diff.md

      - uses: marocchino/sticky-pull-request-comment@v2
        with:
          header: yoink-pr-diff
          path: diff.md
```

## Why this is good

- **Fast feedback.** Reviewers see the deploy plan in the PR thread itself, no need to mentally simulate.
- **Catches the silent-redeploy class of bugs.** A v0.5.0-style change that flips a default and reroles every service is impossible to miss when the dry-run says "6 to update."
- **Composes with the deploy.** The same `yoink up --dry-run` powers the local `yoink diff <service>` command — operators can rerun the same check at their terminal before merging.

## Limitations

- Dry-run reads from the host (it computes the diff against running containers), so the PR runner needs the same auth path your deploy runner does — tailnet membership + whatever secret-resolution your `provider:` setup needs (`YOINK_AGE_KEY` for age, the configured manager's CLI + token for `command`).
- The `--tag` overrides have to match what your deploy workflow will pass. If staging/prod diverge, run two dry-runs against the right host set.

## See also

- [AGE secrets in GitHub Actions](/docs/how-to/age-in-github-actions) — wiring `YOINK_AGE_KEY` into the runner so dry-run can decrypt.
- [Driving yoink from an AI agent](/docs/guide/ai-agents) — how the dry-run output reads to a reviewer (human or otherwise).
