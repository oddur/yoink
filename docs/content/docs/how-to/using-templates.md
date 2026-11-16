---
title: Adding a service via `yoink add`
description: Select a vetted template such as postgres, redis or meilisearch, answer the prompts, and get a service fragment with sealed secrets.
weight: 16
---

`yoink add` renders a template into a service fragment in your project and seals the secrets that the template generates. Bundled templates and third-party repos use the same code path.

To write your own template, see [Write a template for `yoink add`](/docs/how-to/write-a-template).

## Add a bundled template

### Run `yoink add` [step]

```sh
yoink add postgres            # accessory: postgres on a named volume
yoink add redis               # accessory: redis with secure defaults
yoink add meilisearch         # accessory: full-text search w/ sealed master key
yoink add rustfs              # accessory: self-hosted S3-compatible object storage
yoink add restic-backups      # accessory: nightly volume backups to any S3
yoink add openclaw --up       # app: render fragment + run `yoink up`
```

For the list of bundled templates, see [`yoink add` and the bundled templates](/docs/reference/yoink-add#bundled-templates).

### Answer the prompts [step]

yoink prompts for each variable that you did not set with `--var`. Press Enter to accept the default that the prompt shows.

To use every default with no prompts, pass `--yes`.

### Accept the diff [step]

yoink shows the files that it writes, the secrets that it seals, and the `include:` change. Read this summary. Then answer the `Proceed?` prompt.

### Read the notes [step]

yoink prints the notes of the template. The notes usually tell you how to connect the service to your app.

For the full sequence that `yoink add` runs, see [What `yoink add` does](/docs/reference/yoink-add#what-yoink-add-does).

## Pin a template to a version

`yoink add postgres` resolves to `oddur/yoink@main` by default. To pin a tag or a commit, add `@` and the ref:

```sh
yoink add postgres@v0.12.0
yoink add postgres@a1b2c3d
```

yoink caches the template by its resolved SHA. A second run with the same ref uses the cache and no network.

yoink also caches the SHA of a branch or tag. If the branch changed, pass `--refresh` to resolve it again:

```sh
yoink add postgres --refresh
```

## Add a third-party template

Use the form `gh:owner/repo[@ref]/path`. The path is the directory in the repo that holds `template.yaml`. In these examples, that directory is `clickhouse/` at the repo root. yoink adds the `templates/` prefix only to a bundled name.

```sh
yoink add gh:acme/yoink-templates/clickhouse
yoink add gh:acme/yoink-templates@v1.0.0/clickhouse
yoink add gh:acme/yoink-templates@a1b2c3d/clickhouse
```

The prompts and the confirmation are the same as for bundled templates. The confirmation header shows the resolved short SHA.

## Run in CI

Set each variable with `--var key=value`. Pass `--yes` to skip the confirmations. `--yes` is required in CI.

```sh
yoink add postgres --yes \
  --var service_name=db \
  --var memory=1g
```

In non-interactive mode, a variable with no default and no `--var` stops the command with an error.

## Troubleshooting

| Error | Cause and correction |
|---|---|
| "variable X has no default" | Pass `--var X=value`, or run interactively. |
| "couldn't resolve …@main" | yoink cannot reach the GitHub API, and it has no cached SHA for this ref. Pass `@<sha>`. |
| "rendered file … failed yoink validation" | The template has a bug. Report it, or open a PR if the template is bundled. |

## See also

- [`yoink add` and the bundled templates](/docs/reference/yoink-add): the sequence that `yoink add` runs and the table of bundled templates.
- [Templates (`yoink add`)](/docs/explanation/templates): how versions resolve and why the container defaults are strict.
- [TanStack Start + postgres from scratch](/docs/tutorials/tanstack-stack): `yoink add` in an end-to-end tutorial.
- [Volume backups](/docs/how-to/volume-backups): the `restic-backups` and `rustfs` templates together.
