---
title: Drop-in templates with `yoink add`
weight: 3
---

`yoink add <name>` is the "poor man's helm" for accessories and full apps:
fetch a vetted template from GitHub, fill in a few variables, get a sealed
secret + a working service fragment, optionally deploy. Same code path
serves bundled templates and arbitrary 3rd-party repos.

```sh
yoink add postgres            # accessory: postgres on a named volume
yoink add redis               # accessory: redis with secure defaults
yoink add meilisearch         # accessory: full-text search w/ sealed master key
yoink add openclaw --up       # app: render fragment + run `yoink up`
```

## What it does

1. Fetches `templates/<name>/` from `oddur/yoink@main` over GitHub (or any
   repo you point it at — see below).
2. Reads the template's `template.yaml` manifest.
3. Asks for any variables that don't have a default (or, with `--yes`,
   uses every default — useful in CI).
4. Renders the template files with [minijinja](https://github.com/mitsuhiko/minijinja).
5. Generates and **seals** any declared secrets straight into your
   `secrets.age` (the random bytes never leave the local process).
6. Adds the fragment glob to your `yoink.yaml` `include:` list if it
   isn't already covered.
7. Prints the manifest's notes — usually a one-liner showing how to
   wire the new service into your existing app.

Every step gates on a confirmation diff in interactive mode. `--yes`
skips all prompts and is required in CI.

## Bundled templates

| Template     | Kind      | What you get                                                                |
|--------------|-----------|-----------------------------------------------------------------------------|
| `postgres`   | accessory | Postgres + volume + `<NAME>_PASSWORD` sealed                                |
| `redis`      | accessory | Redis 7 with secure-by-default options                                      |
| `meilisearch`| accessory | Meilisearch + volume + sealed master key                                    |
| `openclaw`   | app       | Placeholder app template (rename + repoint `image:` to suit)                |

The full set lives at <https://github.com/oddur/yoink/tree/main/templates>.

## Pinning to a specific version

By default, `yoink add postgres` resolves the bundled template at
`oddur/yoink@main`. To pin to a tag or commit:

```sh
yoink add postgres@v0.12.0
yoink add postgres@a1b2c3d
```

The cache is content-addressed by the resolved commit SHA, so
re-running with the same pin is a no-op (no network).

To bypass the `main → SHA` mapping cache (e.g. when a branch was just
updated and you want the latest):

```sh
yoink add postgres --refresh
```

## 3rd-party templates

Anyone can author templates and publish them in any GitHub repo. Use
the `gh:` prefix to point at one:

```sh
yoink add gh:acme/yoink-templates/clickhouse
yoink add gh:acme/yoink-templates@v1.0.0/clickhouse
yoink add gh:acme/yoink-templates@a1b2c3d/clickhouse
```

The same diff/confirm flow applies to 3rd-party sources — even bundled
templates are run through it. The confirmation header shows the
resolved short SHA so you know exactly what version you're applying.

### Authoring a template

A template is a directory with a small manifest + one or more rendered files. Push to any GitHub repo and consumers get one-line `yoink add gh:you/repo/yourname`. See [Authoring templates for `yoink add`](/docs/recipes/authoring-templates) for the full guide — manifest schema, variable types, secret generation, the container-hardening overrides that bite in practice, and the local-dev iteration loop.

## CI / non-interactive

Every variable can be set via `--var key=value`, and `--yes` skips
confirmations:

```sh
yoink add postgres --yes \
  --var service_name=db \
  --var version=17 \
  --var memory=1g
```

Variables without a default fail fast in non-interactive mode, so
typos surface as clear errors instead of silently using empty values.

## Troubleshooting

- **"variable X has no default"**: pass `--var X=value` or run
  interactively.
- **"couldn't resolve …@main"**: GitHub API unreachable; if you've added
  this template before with the same ref, `yoink add` falls back to
  the cached SHA. Otherwise pass a pinned `@<sha>`.
- **"rendered file failed yoink validation"**: bug in the template;
  please report it (or open a PR if it's one of the bundled ones).
