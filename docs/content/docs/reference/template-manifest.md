---
title: Template manifest
description: "The fields of template.yaml for yoink add, the rules for variables, files and secrets, and how template refs resolve."
---

A template for `yoink add` is a directory with a `template.yaml` manifest and one or more template files. yoink renders the files through [minijinja](https://github.com/mitsuhiko/minijinja).

## Manifest reference

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Human-readable identifier. Error messages also use it. |
| `kind` | `accessory` \| `app` | no | Default: `accessory`. `app` changes the default of the "Deploy now?" prompt to yes. |
| `description` | string | no | One line of text. `yoink add` does not show it. |
| `yoink_min_version` | string | no | `yoink add` stops with an error if the running yoink is older. Loose semver. yoink ignores non-numeric suffixes (`-rc1`). |
| `variables` | list | no | See [Variables](#variables). |
| `files` | list | yes | At least one. See [Files](#files). |
| `secrets` | list | no | See [Secrets](#secrets). |
| `include_glob` | string | no | Rendered through minijinja. If the `include:` list of your `yoink.yaml` does not cover the rendered files, yoink offers to add this glob. |
| `notes` | string | no | Text that yoink prints after a successful `add`. Rendered through minijinja. |
| `connection` | map | no | See [Connection](#connection). |

## Variables

```yaml
variables:
  - name: service_name           # required — used in {{ service_name }}
    prompt: Service name         # what the wizard shows; defaults to `name`
    default: clickhouse          # always provide one — `--yes` (CI) needs it
    pattern: "^[a-z][a-z0-9-]*$" # anchored regex; wizard re-prompts on miss
  - name: version
    default: "24"
    choices: ["23", "24"]        # numbered picker in interactive mode
```

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | The identifier in the `{{ … }}` substitutions in the manifest and in the template files. |
| `prompt` | no | The text that the wizard shows. The default is `name`. |
| `default` | no | The value that the wizard pre-fills, and the value that `yoink add --yes` uses. A non-interactive run stops with an error when a variable has no default and no `--var` override. |
| `choices` | no | The closed set of answers. The wizard shows a numbered picker. A `--var name=X` override must be in the list. |
| `pattern` | no | A regex that the full value must match. On a mismatch, the wizard prompts again, and a non-interactive run stops with an error. |

`pattern` uses a small matcher that is a part of yoink. It supports literals, `.`, character classes such as `[a-z0-9-]`, `*`, `+`, and top-level alternation such as `^(true|false)$`.

Templates can use the minijinja filters and conditionals on a variable:

- Filters: `{{ name | upper }}`, `{{ name | lower }}`, `{{ name | default("fallback") }}`.
- Conditionals: `{% if domain %}domain: {{ domain }}{% endif %}`.

## Files

```yaml
files:
  - dest: "services/{{ service_name }}.yaml"
    template: service.yaml.tmpl
  - dest: "config/{{ service_name }}.toml"
    template: config.toml.tmpl
```

| Key | Meaning |
|---|---|
| `dest` | The path of the file that yoink writes, relative to the directory of your `yoink.yaml`. Rendered through minijinja, so `{{ service_name }}` gives one file for each instance. yoink rejects an absolute path and a path that contains `..`. |
| `template` | The path of the source file in the template directory. yoink renders its contents through minijinja. |

Each rendered file must parse as a yoink [`ConfigFragment`](/docs/reference/config), the shape that you write by hand in `yoink.yaml`. yoink makes sure of this before it writes a file. A bad template thus writes no broken config.

## Secrets

```yaml
secrets:
  - name: "{{ service_name | upper }}_PASSWORD"
    generate: random:32
  - name: "{{ service_name | upper }}_ADMIN_TOKEN"
    generate: random:48
```

| Key | Meaning |
|---|---|
| `name` | The key in the sealed bundle. Rendered through minijinja. |
| `generate` | Only `random:N` (8 ≤ N ≤ 512). yoink reads N random bytes locally and encodes them as base32. |

The convention for `name` is `{{ service_name | upper }}_<KIND>`. yoink seals each value into `secrets.age`. The value does not leave the local process in plaintext.

The secrets config of your `yoink.yaml` decides what `yoink add` does with a template that has secrets:

| `secrets:` in `yoink.yaml` | Behavior |
|---|---|
| `provider: age` | yoink seals the values. |
| Absent | An interactive run offers to bootstrap age. A non-interactive run stops with an error. |
| `provider: command` | `yoink add` stops with an error. yoink cannot write to an external store. |

## Connection

`connection` holds config that a different service needs to connect to this one. After a successful `add`, yoink prints it as a YAML block that you can paste into the consuming service. yoink renders each value through minijinja.

| Key | Type | Meaning |
|---|---|---|
| `env` | map | Plain env vars. |
| `env_from_secrets` | map | Env var name to sealed-secret name. |
| `depends_on` | list | Service names for the `depends_on:` list of the consuming service. |

## Template refs

| Ref | Example | Resolution |
|---|---|---|
| Branch | `@main` | yoink resolves it to a SHA one time and caches the `branch → SHA` mapping. Without `--refresh`, the cached mapping wins. |
| Tag | `@v1.0.0` | Same as a branch. |
| SHA | `@a1b2c3d` | Content-addressed. yoink never resolves it again and never fetches it again after it is in the cache. |

`--refresh` makes yoink resolve a branch or a tag to its latest commit SHA again.

## Cache and download limits

| Item | Value |
|---|---|
| Transport | Gzipped tarball from `https://codeload.github.com/...` |
| Cache path | `~/.cache/yoink/templates/<owner>__<repo>__<sha>/` |
| Download cap | 50 MB |
| Unpacked cap | 200 MB |

The cache is content-addressed by SHA, so a new fetch after `rm -rf ~/.cache/yoink/templates` is harmless.

## See also

- [Templates (`yoink add`)](/docs/explanation/templates): why templates work this way.
- [Write a template for `yoink add`](/docs/how-to/write-a-template): a complete example and the development loop.
- [Publish a template](/docs/how-to/publish-a-template): repo layouts, pins, and the bundled set.
- [Adding a service via `yoink add`](/docs/how-to/using-templates): how to use a template.
- [Configuration reference](/docs/reference/config): the schema for the rendered service fragment.
