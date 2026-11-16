---
title: Publish a template
description: "Choose a repo layout for a yoink add template, tell consumers which ref to pin, and submit a template to the bundled set."
---

Any GitHub repo can hold a template. To write the template first, see [Write a template for `yoink add`](/docs/how-to/write-a-template).

## Choose a repo layout

A consumer ref has the form `gh:owner/repo[@ref]/path`. The path is the directory in the repo that holds `template.yaml`. The path cannot be empty, so put the template in a subdirectory.

| Layout | Consumer ref | Use it when |
|---|---|---|
| One repo for each template | `gh:yourname/yoink-template-clickhouse/clickhouse` | You want a template that is easy to advertise, to vendor, and to fork. |
| A templates monorepo, with one subdirectory for each template | `gh:yourname/yoink-templates/clickhouse` | You maintain several templates and want less overhead. |
| A directory in your existing project | `gh:yourname/your-app/templates/your-app` | The template is specific to your project and lives next to the code that it deploys. |

yoink downloads the full repo tarball. It caps the download at 50 MB and the unpacked size at 200 MB. If your repo is larger, move the template to its own repo.

## Tell consumers which ref to pin

1. In your README, write the ref that you recommend: `main` for the latest changes, or a tag for stability.
2. If the template needs a yoink feature from a later version, set `yoink_min_version: "X.Y.Z"` in the manifest.

With `yoink_min_version:` set, an older yoink stops and tells the consumer to upgrade yoink or to pin an older template. Without it, an older yoink can render a wrong config with no error.

For how branch, tag, and SHA refs resolve, see [Template refs](/docs/reference/template-manifest#template-refs).

## Submit a template to the bundled set

The six bundled templates (`postgres`, `redis`, `meilisearch`, `rustfs`, `restic-backups`, `openclaw`) are in the yoink repo at [`/templates/<name>/`](https://github.com/oddur/yoink/tree/main/templates). A bare name, for example `yoink add postgres`, resolves to `templates/postgres` in `oddur/yoink`. PRs that add a bundled template are welcome. For the bar that a new template must pass, see [The bundled set](/docs/explanation/templates#the-bundled-set).

A PR must meet these four acceptance criteria:

1. **The template renders valid YAML.** The `tests/add_integration.rs` test walks `templates/*/template.yaml`. Make sure that the test passes with your template.
2. **The template passes a smoke test on a real host.** On a real Docker host, run `yoink init`, then `yoink add <yourname>`, then `yoink up`. Then use the service end-to-end. Put the output in the PR.
3. **The hardening overrides are documented.** If you needed `user: "0:0"`, `cap_add: [...]`, or `read_only: false`, explain why in a comment in `service.yaml.tmpl`. Name the entrypoint operation and the file.
4. **The secret names agree.** This criterion applies if your template seals a secret. Each `env_from_secrets:` reference in the rendered fragment must use a name that `secrets[].name:` produces. The integration test finds mismatches.

These criteria are not required for a third-party template in your own repo.

## See also

- [Write a template for `yoink add`](/docs/how-to/write-a-template): the example template and the development loop.
- [Template manifest](/docs/reference/template-manifest): the manifest fields, the ref rules, and the download limits.
- [Templates (`yoink add`)](/docs/explanation/templates): how versions resolve and what the bundled set is for.
- [Adding a service via `yoink add`](/docs/how-to/using-templates): how consumers add your template.
