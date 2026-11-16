---
title: Pre-merge dry-run on every PR
description: Post a sticky GitHub PR comment that shows what yoink up changes before the merge.
weight: 7
---

`yoink up --dry-run --format=markdown` connects to each host and compares the running containers with the spec that a deploy applies. It changes nothing and prints a Markdown summary. In a sticky GitHub PR comment, the summary shows reviewers what the deploy changes before the merge.

## Prerequisites

The PR runner has the same auth path as your deploy runner, because the dry-run reads from the hosts:

- Tailnet membership.
- The secret resolution that your `provider:` config needs. For `age`, that is `YOINK_AGE_KEY`. For `command`, that is the CLI and the token of the configured manager.

## Add the workflow

### Create the workflow file [step]

Create `.github/workflows/yoink-pr-diff.yml`:

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

### Match the `--tag` overrides [step]

Set the `--tag` overrides to the same overrides that your deploy workflow passes.

If staging and prod have different configs, run one dry-run for each config.

## Read the comment

The comment looks like this:

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

The comment shows which services this PR rolls and which services stay the same. Make sure that the image change is the change that you expect.

The plan line also shows a silent redeploy. For example, a yoink upgrade that changes a default and rolls every service again shows as "6 to update."

## Run the same dry-run locally

Before you merge, run `yoink up --dry-run` in your terminal with the same `--tag` overrides. For one service, `yoink diff <service>` compares the tag of each running container with the target tag.

## See also

- [AGE secrets in GitHub Actions](/docs/how-to/age-in-github-actions): how to give `YOINK_AGE_KEY` to the runner so that the dry-run can decrypt.
- [Run staging alongside prod](/docs/how-to/staging-alongside-prod): two configs on the same hosts, each with its own dry-run.
- [Driving yoink from an AI agent](/docs/explanation/ai-agents): how a reviewer, human or not, reads the dry-run output.
- [CLI reference](/docs/reference/cli): the `--dry-run`, `--format` and `--tag` flags.
