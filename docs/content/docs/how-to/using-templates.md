---
title: Adding a service via `yoink add`
description: Pick a vetted template (postgres, redis, meilisearch, …), answer prompts, get a sealed-secret-ready service fragment.
weight: 16
---

Pick a template, answer the prompts, get a service fragment with sealed secrets dropped into your project. Same code path for bundled templates and 3rd-party repos.

To publish your own, see [Authoring templates](/docs/guide/templates).

## Try it

```sh
yoink add postgres            # accessory: postgres on a named volume
yoink add redis               # accessory: redis with secure defaults
yoink add meilisearch         # accessory: full-text search w/ sealed master key
yoink add rustfs              # accessory: self-hosted S3-compatible object storage
yoink add restic-backups      # accessory: nightly volume backups to any S3
yoink add openclaw --up       # app: render fragment + run `yoink up`
```

## What it does

1. Fetches `templates/<name>/` from `oddur/yoink@main` (or another repo; see below).
2. Reads the template's `template.yaml` manifest.
3. Prompts for variables without a default. `--yes` uses every default.
4. Renders the files with [minijinja](https://github.com/mitsuhiko/minijinja).
5. Generates and **seals** declared secrets into your `secrets.age` — random bytes never leave the local process.
6. Adds the fragment glob to your `yoink.yaml` `include:` list if not already covered.
7. Prints the manifest's notes, usually how to wire the service into your app.

Every step gates on a confirmation diff. `--yes` skips prompts (required in CI).

## Bundled templates

| Template | Kind | What you get |
|---|---|---|
| `postgres` | accessory | Postgres 18 + named volume + `<NAME>_PASSWORD` sealed; opt-in WAL archiving for PITR |
| `redis` | accessory | Redis 7 with secure-by-default options |
| `meilisearch` | accessory | Meilisearch + volume + sealed master key |
| `rustfs` | accessory | Apache-2.0 S3-compatible object storage; sealed root credentials |
| `restic-backups` | accessory | Nightly volume snapshots via resticker (restic + go-cron) to any S3-compatible bucket |
| `openclaw` | app | Placeholder app template (rename + repoint `image:` to suit) |

The full set lives at <https://github.com/oddur/yoink/tree/main/templates>.

## Pinning to a specific version

`yoink add postgres` resolves to `oddur/yoink@main` by default. Pin to a tag or commit:

```sh
yoink add postgres@v0.12.0
yoink add postgres@a1b2c3d
```

The cache is content-addressed by resolved SHA — re-running with the same pin is a no-op (no network).

To bypass the `main → SHA` cache after a branch update:

```sh
yoink add postgres --refresh
```

## 3rd-party templates

Use the `gh:` prefix:

```sh
yoink add gh:acme/yoink-templates/clickhouse
yoink add gh:acme/yoink-templates@v1.0.0/clickhouse
yoink add gh:acme/yoink-templates@a1b2c3d/clickhouse
```

Same diff/confirm flow as bundled templates. The confirmation header shows the resolved short SHA.

## CI / non-interactive

Set variables via `--var key=value`; `--yes` skips confirmations:

```sh
yoink add postgres --yes \
  --var service_name=db \
  --var memory=1g
```

Missing variables fail fast in non-interactive mode rather than defaulting to empty.

## Troubleshooting

- **"variable X has no default"**: pass `--var X=value` or run interactively.
- **"couldn't resolve …@main"**: GitHub API unreachable. Falls back to the cached SHA if you've added this template before with the same ref; otherwise pass `@<sha>`.
- **"rendered file failed yoink validation"**: template bug; report it (or open a PR for bundled templates).

## See also

- [Authoring templates](/docs/guide/templates) — the manifest schema, minijinja rendering, secrets sealing, publishing patterns.
- [TanStack Start + postgres from scratch](/docs/how-to/tanstack-stack) — `yoink add` in an end-to-end walkthrough.
- [Volume backups](/docs/how-to/volume-backups) — pairing the `restic-backups` and `rustfs` templates.
