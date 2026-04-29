---
title: Adding a service via `yoink add`
weight: 16
---

Pick a vetted template, answer a few prompts, get a sealed-secret-having service fragment dropped into your project. Same code path serves bundled templates and arbitrary 3rd-party repos.

For the author side (publish your own template that anyone can `yoink add`), see [Authoring templates](/docs/guide/templates).

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

1. Fetches `templates/<name>/` from `oddur/yoink@main` over GitHub (or any repo you point it at — see below).
2. Reads the template's `template.yaml` manifest.
3. Asks for any variables that don't have a default (or, with `--yes`, uses every default — useful in CI).
4. Renders the template files with [minijinja](https://github.com/mitsuhiko/minijinja).
5. Generates and **seals** any declared secrets straight into your `secrets.age` (the random bytes never leave the local process).
6. Adds the fragment glob to your `yoink.yaml` `include:` list if it isn't already covered.
7. Prints the manifest's notes — usually a one-liner showing how to wire the new service into your existing app.

Every step gates on a confirmation diff in interactive mode. `--yes` skips all prompts and is required in CI.

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

By default, `yoink add postgres` resolves the bundled template at `oddur/yoink@main`. To pin to a tag or commit:

```sh
yoink add postgres@v0.12.0
yoink add postgres@a1b2c3d
```

The cache is content-addressed by the resolved commit SHA, so re-running with the same pin is a no-op (no network).

To bypass the `main → SHA` mapping cache (e.g. when a branch was just updated and you want the latest):

```sh
yoink add postgres --refresh
```

## 3rd-party templates

Anyone can author templates and publish them in any GitHub repo. Use the `gh:` prefix to point at one:

```sh
yoink add gh:acme/yoink-templates/clickhouse
yoink add gh:acme/yoink-templates@v1.0.0/clickhouse
yoink add gh:acme/yoink-templates@a1b2c3d/clickhouse
```

The same diff/confirm flow applies to 3rd-party sources — even bundled templates are run through it. The confirmation header shows the resolved short SHA so you know exactly what version you're applying.

## CI / non-interactive

Every variable can be set via `--var key=value`, and `--yes` skips confirmations:

```sh
yoink add postgres --yes \
  --var service_name=db \
  --var memory=1g
```

Variables without a default fail fast in non-interactive mode, so typos surface as clear errors instead of silently using empty values.

## Troubleshooting

- **"variable X has no default"**: pass `--var X=value` or run interactively.
- **"couldn't resolve …@main"**: GitHub API unreachable; if you've added this template before with the same ref, `yoink add` falls back to the cached SHA. Otherwise pass a pinned `@<sha>`.
- **"rendered file failed yoink validation"**: bug in the template; please report it (or open a PR if it's one of the bundled ones).

## See also

- [Authoring templates](/docs/guide/templates) — the manifest schema, minijinja rendering, secrets sealing, publishing patterns.
- [TanStack Start + postgres from scratch](/docs/how-to/tanstack-stack) — `yoink add` in an end-to-end walkthrough.
- [Volume backups](/docs/how-to/volume-backups) — pairing the `restic-backups` and `rustfs` templates.
