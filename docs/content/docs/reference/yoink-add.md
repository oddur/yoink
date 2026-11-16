---
title: "`yoink add` and the bundled templates"
description: The sequence that yoink add runs for a template, and the list of templates that ship with yoink.
---

## What `yoink add` does

1. Fetches `templates/<name>/` from `oddur/yoink@main`, or from a different repo. `--from-path` reads a local directory.
2. Reads the `template.yaml` manifest of the template.
3. Prompts for each variable that has no `--var` value. `--yes` uses every default.
4. Renders the files with [minijinja](https://github.com/mitsuhiko/minijinja).
5. Shows the planned changes and asks `Proceed?`. Then writes the files.
6. Generates the declared secrets and **seals** them into your `secrets.age`. The random bytes never leave the local process.
7. Adds the fragment glob to the `include:` list of your `yoink.yaml`, if the list does not already cover it.
8. Prints the `connection` block and the notes of the manifest. They tell you how to connect the service to your app.
9. Asks `Deploy now?`. `--up` deploys without the question.

`--yes` skips the prompts and is required in CI.

## Bundled templates

| Template | Kind | What you get |
|---|---|---|
| `postgres` | accessory | Postgres 18, a named volume, and a sealed `<NAME>_PASSWORD`. Opt-in WAL archiving for PITR. |
| `redis` | accessory | Redis 7 with secure-by-default config. |
| `meilisearch` | accessory | Meilisearch, a volume, and a sealed master key. |
| `rustfs` | accessory | Apache-2.0 S3-compatible object storage with sealed root credentials. |
| `restic-backups` | accessory | Nightly volume snapshots through resticker (restic + go-cron) to any S3-compatible bucket. |
| `openclaw` | app | Placeholder app template. Rename it and point `image:` at your image. |

The full set is at <https://github.com/oddur/yoink/tree/main/templates>.

## See also

- [Adding a service via `yoink add`](/docs/how-to/using-templates): the commands for bundled, pinned and third-party templates.
- [Authoring templates](/docs/explanation/templates): the manifest, minijinja rendering, and secrets sealing.
- [CLI reference](/docs/reference/cli): the flags of `yoink add`.
