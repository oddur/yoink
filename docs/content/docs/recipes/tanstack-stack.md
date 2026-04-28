---
title: TanStack Start + postgres from scratch
weight: 2
---

A TanStack Start app deploying alongside a managed postgres, built locally and shipped without a registry. Five files, three commands, working stack on the host in about ten minutes. Adding more accessories (redis, meilisearch, …) is the same shape — `yoink add <name>`, paste the connection block.

This is the "indie one-shot" pattern — single laptop, single host, no CI, no Docker Hub account, no Vault. Public exposure (HTTPS, domains) is intentionally out of scope here; the [port-forward recipe](/docs/recipes/port-forward) covers reaching the deployed stack from your laptop.

## What you need

- A host with Docker installed and key-based ssh login. A fresh Hetzner / DigitalOcean / Linode box qualifies.
- yoink, node, and docker on your laptop.

> **Local-only iteration?** yoink is a remote-deploy tool by design — destructive commands target ssh hosts, not your laptop's docker daemon (read-only commands like `yoink tui` / `yoink status` do work locally). For pure local development loops, reach for Docker Compose; come back to yoink when you have a host to ship to. If you genuinely want yoink against a "local" VM, expose its docker daemon over SSH (Colima, OrbStack) and treat it as a remote host — no special syntax.

## Step 1: scaffold the app

```sh
npm create @tanstack/start@latest my-app
cd my-app
npm install pg
```

The scaffolder produces a Vite-backed Nitro SSR setup. We add `pg` for the demo integration.

Drop the database client into a server-only module so the secrets never reach the browser bundle:

```ts
// src/lib/db.server.ts — the .server.ts suffix keeps this off the client
import { Client } from 'pg'

export type DbStatus =
  | { ok: true; version: string }
  | { ok: false; error: string }

export async function checkStack(): Promise<DbStatus> {
  const pgUrl = `postgres://${process.env.POSTGRES_USER}:${
    process.env.POSTGRES_PASSWORD
  }@${process.env.POSTGRES_HOST}:5432/${process.env.POSTGRES_DB}`

  const pg = new Client({ connectionString: pgUrl })
  try {
    await pg.connect()
    const r = await pg.query('SELECT version()')
    return { ok: true, version: r.rows[0].version }
  } catch (e) {
    return { ok: false, error: (e as Error).message }
  } finally {
    await pg.end().catch(() => {})
  }
}
```

Wire it into the index route so the loader fetches at request time:

```tsx
// src/routes/index.tsx
import { createFileRoute } from '@tanstack/react-router'
import { createServerFn } from '@tanstack/react-start'
import { checkStack, type DbStatus } from '../lib/db.server'

const getStackStatus = createServerFn({ method: 'GET' }).handler(checkStack)

export const Route = createFileRoute('/')({
  component: App,
  loader: () => getStackStatus(),
})

function App() {
  const status = Route.useLoaderData() as DbStatus
  return (
    <main>
      <h1>yoink stack demo</h1>
      <pre>{JSON.stringify(status, null, 2)}</pre>
    </main>
  )
}
```

## Step 2: Dockerfile

Multi-stage build. Nitro bundles all server-side deps into `.output/server/_libs/`, so the runtime image needs no `node_modules`:

```dockerfile
# Dockerfile
FROM node:22-alpine AS builder
WORKDIR /app
COPY package*.json ./
RUN npm install --no-audit --no-fund
COPY . .
RUN npm run build

FROM node:22-alpine
WORKDIR /app
COPY --from=builder /app/.output ./.output
ENV PORT=3000 HOST=0.0.0.0 NODE_ENV=production
EXPOSE 3000
CMD ["node", ".output/server/index.mjs"]
```

`EXPOSE 3000` is the cue `yoink init` will pick up automatically. Add a `.dockerignore`:

```
node_modules
.output
.git
secrets.age
yoink.yaml
services/
```

## Step 3: yoink init

```sh
yoink init root@your-host
```

`init` reads your `Dockerfile`, infers the service name + port + image, generates a starter `yoink.yaml`, and bootstraps an age identity for sealed secrets:

```
✓ wrote yoink.yaml (18 lines, validates clean)

inferred:
  service   my-app                          (cwd)
  image     my-app                          (Dockerfile (bare name))
  host      root@your-host                  (positional arg)
  port      3000 with /health healthcheck   (Dockerfile EXPOSE)

⚠  BACK UP THIS KEY  …
  identity: ~/.config/yoink/keys/age1….key
```

**Back up the key now** (paste into a password manager) — see [Sealed secrets](/docs/recipes/sealed-secrets) for backup options. Lose it and the sealed values in this repo are gone.

## Step 4: drop in postgres

```sh
yoink add postgres
```

Generates a fragment in `services/postgres.yaml` (postgres:18-alpine + sealed `POSTGRES_PASSWORD`), seals the password into `secrets.age`, and extends `yoink.yaml`'s `include:` list.

After the add succeeds, yoink prints a **paste-ready connection block** that uses only fields already in the schema (`depends_on:`, `env:`, `env_from_secrets:`):

```
connect another service to postgres:
  # paste under the consuming service in yoink.yaml,
  # rename keys to whatever your app expects.
  depends_on: [postgres]
  env:
    POSTGRES_DB: app
    POSTGRES_HOST: postgres
    POSTGRES_PORT: "5432"
    POSTGRES_USER: app
  env_from_secrets:
    POSTGRES_PASSWORD: POSTGRES_PASSWORD
```

The keys are the accessory's suggestion — neutral, conventional. If your app reads a different shape (e.g. `DATABASE_URL`, `PG_HOST`), rename them in the paste; yoink doesn't care which env names the consuming service uses, only that they're present.

> **Need redis, meilisearch, …?** Same shape: `yoink add redis` (or `yoink add meilisearch`) prints its own connection block. Paste alongside this one, merging the `depends_on:` lists.

## Step 5: wire the app to the accessory

Paste the `connect another service to …` block into your app's `services[]` entry. The keys are conventional but not magic — drop any your app doesn't read (the demo `db.server.ts` ignores `POSTGRES_PORT` since the port is hard-coded).

Then add the `build:` block. The whole file ends up looking like:

```yaml
hosts:
  - { address: your-host, user: root }

secrets:
  provider: age
  recipients:
    - age1…                       # generated by `yoink init`

services:
  - name: my-app
    image: my-app
    tag: latest
    build:
      context: .
      # Cross-build to amd64 when developing on Apple Silicon.
      # Drop this if your laptop matches the host's arch.
      extra_args: ["--platform", "linux/amd64"]
    depends_on: [postgres]
    env:
      POSTGRES_HOST: postgres
      POSTGRES_DB: app
      POSTGRES_USER: app
    env_from_secrets:
      POSTGRES_PASSWORD: POSTGRES_PASSWORD
    run:
      port: 3000
      healthcheck_path: /

include:
  - "services/*.yaml"
```

`yoink validate` confirms the config parses; `yoink doctor` checks deploy-readiness (unreachable host, missing identity, etc.).

## Step 6: deploy

```sh
yoink up --build
```

One command. The host pulls postgres from Docker Hub; the app (with its `build:` block) is built locally and shipped to the host via [unregistry](https://github.com/psviderski/unregistry) over SSH — no registry account required for the app. Both containers come up on the `yoink` network, the app's healthcheck gates the swap.

First deploy is the slow one — pulling Docker Hub's `node:22-alpine` and the npm install. Re-deploys are fast: unregistry ships only changed layers.

## Verify

The app is reachable on the host's docker network at `my-app:3000` but isn't bound to a host port — that's the secure default. To open it on your laptop:

```sh
yoink pf my-app -o
```

`yoink pf` opens a tunnel to the container even though no host port is published, and `-o` opens the printed URL in your browser. See the [port-forward recipe](/docs/recipes/port-forward) for how it works.

The page should render JSON with `ok: true` and a postgres version string.

## Re-deploys

```sh
yoink up --build --service my-app
```

~15 seconds for a typical code change. unregistry's layer dedup means only the rebuilt application layer ships over SSH.

For tag-stamped deploys (so `yoink history` shows commits, not `latest`s):

```sh
yoink up --build --here --service my-app
```

`--here` substitutes `git rev-parse --short HEAD` for the tag.

## What just happened

Five files (`Dockerfile`, `.dockerignore`, `src/lib/db.server.ts`, `src/routes/index.tsx`, `yoink.yaml`) plus three yoink commands (`init`, `add`, `up`) brought up two containers on a private docker network with a sealed-on-commit Postgres password and drift detection on every container.

- [Sealed secrets](/docs/recipes/sealed-secrets) — how the `POSTGRES_PASSWORD` is generated and decrypted at deploy.
- [Architecture](/docs/guide/architecture) — drift detection, healthcheck-gated swap.
- [CLI reference](/docs/reference/cli) — every `yoink up` flag.

## Going public — HTTPS with Let's Encrypt

The recipe above stays internal so you can verify the stack without owning a domain. Once you have one and an A record pointing at the host, swapping `yoink pf` for public HTTPS is a two-line edit:

```yaml
# yoink.yaml — top-level
proxy:
  email: you@example.com           # Let's Encrypt registration / expiry warnings

services:
  - name: my-app
    # …everything else stays the same…
    domain: app.example.com        # ← the new line
```

`yoink up` notices a `domain:`-tagged service and synthesizes a `yoink-proxy` running `caddy:2` alongside your app. Caddy joins the same docker network, requests a Let's Encrypt cert (HTTP-01 against `app.example.com`), and routes incoming traffic to `my-app:3000` automatically. No `publish:` on the app — only the proxy binds host ports `:80` / `:443`.

Two things to check before deploying:

1. **DNS** — `dig +short A app.example.com` must resolve to the host's public IP. Let's Encrypt validates over HTTP-01; an unpointed record produces self-signed certs and a permanent browser warning.
2. **`proxy.email:`** — yoink doctor warns when this is the docs placeholder. Use a real address you read; LE expiry warnings go there.

Re-deploy and the app is live at `https://app.example.com`. `yoink doctor` confirms ahead of `yoink up` that the host is reachable, the domain resolves, and the email isn't a placeholder.

For Cloudflare-fronted setups (so you don't need an open `:80` for ACME), see [Cloudflare Origin Certificates](/docs/recipes/cloudflare-origin-certs). For multi-host shared ACME storage, see [Multi-host Let's Encrypt with Redis](/docs/recipes/multi-host-redis-storage).

## Multiple projects on one host

Pattern that scales: **one `yoink.yaml` per host, multiple service fragments**. The host's central yaml aggregates services from each project repo via the `include:` mechanism.

```yaml
# host-prod/yoink.yaml — the canonical config for `your-host`
hosts:
  - { address: your-host, user: root }
secrets:
  provider: age
  recipients: [age1…]

include:
  - "services/*.yaml"
  - "../project-a/services/*.yaml"   # mount each project's services
  - "../project-b/services/*.yaml"   # alongside its own repo
```

Each project's repo keeps its `services/<project>.yaml` fragment alongside its code; the host config aggregates them. `yoink up` from the host config sees every project's services as one cluster. Per-project deploys are still possible — `yoink up --service project-a-app` from inside the host config — they just need to run from the directory with the central `yoink.yaml`.

Coordination cost: every project's CI runner needs the host config's age identity (or the per-project services need to ship their own `provider: command` entry that the host config inherits).

## See also

- [Drop-in templates with `yoink add`](/docs/recipes/add-templates) — adding redis / meilisearch alongside postgres.
- [Port-forward to any service](/docs/recipes/port-forward) — full background on the `yoink pf` verify step.
- [Sealed secrets (age)](/docs/recipes/sealed-secrets) — what `yoink init` set up, and how to back up the key.
- [Edit-save-deploy with `--watch`](/docs/recipes/watch-mode) — turn the redeploy command into a save-triggered loop.
