---
title: Write a template for `yoink add`
description: "Create a template manifest and a service template, add the template from a branch, and correct permission failures on the first deploy."
---

The example is a working `clickhouse` template of two files. After you push the files to a repo, `yoink add gh:you/yourrepo/templates/clickhouse` adds the template.

## Before you start

- A GitHub repo that you can push to. Consumers of the template get it from a GitHub commit.
- A host that you can break, or a local Docker daemon, for the test deploy.

All manifest fields are in the [Template manifest](/docs/reference/template-manifest) reference.

## Write and test the template

### Create the manifest [step]

Create this directory layout in the repo. The path in a `gh:` ref is the path of the template directory from the repo root, here `templates/clickhouse`.

```
templates/
└── clickhouse/
    ├── template.yaml
    └── service.yaml.tmpl
```

Write the manifest. The manifest declares the variables, the files, and the secrets.

```yaml
# templates/clickhouse/template.yaml
name: clickhouse
kind: accessory
description: ClickHouse with sealed credentials and a named volume.
yoink_min_version: "0.12.0"

variables:
  - name: service_name
    prompt: Service name
    default: clickhouse
    pattern: "^[a-z][a-z0-9-]*$"
  - name: version
    prompt: ClickHouse major version
    default: "24"
    choices: ["23", "24"]
  - name: memory
    prompt: Memory limit
    default: 1g

files:
  - dest: "services/{{ service_name }}.yaml"
    template: service.yaml.tmpl

secrets:
  - name: "{{ service_name | upper }}_PASSWORD"
    generate: random:32

include_glob: "services/*.yaml"

notes: |
  Connect from another service:
    depends_on: [{{ service_name }}]
    env:
      CLICKHOUSE_URL: tcp://{{ service_name }}:9000
    env_from_secrets:
      CLICKHOUSE_PASSWORD: {{ service_name | upper }}_PASSWORD
```

Give each variable a default. A `yoink add --yes` run (CI) fails when a variable has no default and no `--var` override.

### Create the service template [step]

Write the file that yoink renders through [minijinja](https://github.com/mitsuhiko/minijinja):

```yaml
# templates/clickhouse/service.yaml.tmpl
services:
  - name: {{ service_name }}
    image: clickhouse/clickhouse-server
    tag: "{{ version }}-alpine"
    env:
      CLICKHOUSE_USER: app
      CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT: "1"
    env_from_secrets:
      CLICKHOUSE_PASSWORD: {{ service_name | upper }}_PASSWORD
    run:
      volumes:
        - clickhouse-{{ service_name }}-data:/var/lib/clickhouse
      options:
        memory: {{ memory }}
        # ClickHouse needs root briefly to chown its data volume.
        user: "0:0"
        cap_add: [CHOWN, DAC_OVERRIDE, FOWNER]
        read_only: false
```

Make sure that the `env_from_secrets:` reference uses the same name that `secrets[].name:` in the manifest produces.

### Push to a feature branch [step]

Use a branch name with no `/`. In a `gh:` ref, the first `/` after the repo name ends the ref, so a branch such as `feat/clickhouse-template` does not resolve.

```sh
git push -u origin clickhouse-template
```

### Add the template from the branch [step]

Use the commit SHA, or the branch name with `--refresh`:

```sh
yoink add gh:you/yourrepo@clickhouse-template/templates/clickhouse \
  --refresh --yes
```

`--refresh` makes yoink resolve the branch to its latest commit SHA again. Without `--refresh`, yoink uses the cached `branch → sha` mapping and a stale version of your template.

To test a local copy with no push, pass `--from-path <template-directory>` in place of the `gh:` ref.

### Deploy to a test host [step]

```sh
yoink up --service clickhouse
```

The manifest validation and the validation of the rendered YAML do not find permission failures. These failures occur only when the container starts, so do not skip this step.

If the container starts with no permission failure, the template passes the test. After each change to the template, do the push, add, and deploy steps again.

## Correct a permission failure on the first deploy

The container defaults of yoink are strict, and some popular images need overrides. For the reasons and for worked examples in the bundled templates, see [Hardening overrides](/docs/explanation/templates#hardening-overrides).

1. Run `docker logs <container>` to get the error string.
2. Find the error in this table.
3. Add the override to `run.options:` in the service template.
4. In a comment in the template, explain why the override is necessary.

| Symptom | Likely cause | Override |
|---|---|---|
| `Operation not permitted` during `chmod`/`chown` of a volume | `cap_drop: [ALL]` removes CHOWN/FOWNER | `cap_add: [CHOWN, DAC_OVERRIDE, FOWNER]` |
| `Operation not permitted` during user-switch (gosu, su-exec, and others) | Caps removed, and a `user: nobody` mismatch | `user: "0:0"` + `cap_add: [SETGID, SETUID]` |
| `Read-only file system (os error 30)` mid-request | The app writes to the rootfs (logs, pidfiles, payload buffers) | `read_only: false` |
| `Permission denied` on a write to a volume | Root owns the volume, and the container runs as nobody | `user: "0:0"` (the entrypoint of the image usually lowers its privileges itself) |

## Clear the template cache

The cache is at `~/.cache/yoink/templates/<owner>__<repo>__<sha>/`, or below `$XDG_CACHE_HOME/yoink/templates/` if that variable is set.

If yoink uses an old version of the template, remove the cache. The next `yoink add` fetches the template again.

```sh
rm -rf ~/.cache/yoink/templates
```

## See also

- [Template manifest](/docs/reference/template-manifest): every manifest field, the variable rules, and how refs resolve.
- [Templates (`yoink add`)](/docs/explanation/templates): why the container defaults are strict and how versions resolve.
- [Publish a template](/docs/how-to/publish-a-template): repo layouts, pins, and a PR to the bundled set.
- [Adding a service via `yoink add`](/docs/how-to/using-templates): the page for people who use your template.
