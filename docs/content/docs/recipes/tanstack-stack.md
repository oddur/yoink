---
title: TanStack Start + postgres + redis from scratch
weight: 5
---

A TanStack Start app deploying alongside a managed postgres and redis, built locally and shipped without a registry. Five files of yaml, three commands, real HTTPS.

This is the "indie one-shot" pattern — single laptop, single host, no CI, no Docker Hub account, no Vault. End-to-end works on a fresh repo in about ten minutes.

## What you need

- A host with Docker installed and key-based ssh login. A fresh Hetzner / DigitalOcean / Hetzner Cloud / Linode box qualifies.
- A domain whose A/AAAA record points at that host's public IP. **Set this up before deploying** — Let's Encrypt validates by HTTP-01 against the live IP, so an unpointed domain produces self-signed certs and a permanent browser warning. (You can deploy without `domain:` first and add HTTPS later; both flows work.)
- yoink, node, and docker on your laptop.

## Step 1: scaffold the app

```sh
npm create @tanstack/start@latest my-app
cd my-app
npm install pg ioredis
```

The scaffolder produces a Vite-backed Nitro SSR setup. We add `pg` + `ioredis` for the demo integration.

Drop the database client into a server-only module so the secrets never reach the browser bundle:

```ts
// src/lib/db.server.ts — the .server.ts suffix keeps this off the client
import { Client } from 'pg'
import Redis from 'ioredis'

export type DbStatus = {
  postgres: { ok: true; version: string } | { ok: false; error: string }
  redis: { ok: true; value: string | null } | { ok: false; error: string }
}

export async function checkStack(): Promise<DbStatus> {
  const pgUrl = `postgres://${process.env.POSTGRES_USER}:${
    process.env.POSTGRES_PASSWORD
  }@${process.env.POSTGRES_HOST}:5432/${process.env.POSTGRES_DB}`

  const pg = new Client({ connectionString: pgUrl })
  let postgres: DbStatus['postgres']
  try {
    await pg.connect()
    const r = await pg.query('SELECT version()')
    postgres = { ok: true, version: r.rows[0].version }
  } catch (e) {
    postgres = { ok: false, error: (e as Error).message }
  } finally {
    await pg.end().catch(() => {})
  }

  const redis = new Redis({
    host: process.env.REDIS_HOST,
    port: 6379,
    lazyConnect: true,
    maxRetriesPerRequest: 1,
  })
  let redisStatus: DbStatus['redis']
  try {
    await redis.connect()
    await redis.set('demo', 'it works', 'EX', 60)
    redisStatus = { ok: true, value: await redis.get('demo') }
  } catch (e) {
    redisStatus = { ok: false, error: (e as Error).message }
  } finally {
    redis.disconnect()
  }

  return { postgres, redis: redisStatus }
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

## Step 4: drop in postgres + redis

```sh
yoink add postgres
yoink add redis
```

Each generates a fragment in `services/`, seals any random credentials into `secrets.age`, and extends `yoink.yaml`'s `include:` list. Two files appear:

```
services/postgres.yaml   # postgres:16-alpine + sealed POSTGRES_PASSWORD
services/redis.yaml      # redis:7-alpine, secure-by-default options
```

After each `add` succeeds, yoink prints a **paste-ready connection block** that uses only fields already in the schema (`depends_on:`, `env:`, `env_from_secrets:`):

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

## Step 5: wire the app to the accessories

Paste the two `connect another service to …` blocks into your app's `services[]` entry. Two things to clean up after the paste:

- Each block prints its own `depends_on: [...]`. Merge them into one list (`depends_on: [postgres, redis]`).
- The keys are conventional but not magic — drop any your app doesn't read (the demo `db.server.ts` ignores `POSTGRES_PORT` / `REDIS_PORT` since both are hard-coded).

Then add `domain:`, a real `proxy.email:` (Let's Encrypt sends expiry warnings there — `yoink doctor` flags the placeholder), and the `build:` block. The whole file ends up looking like:

```yaml
hosts:
  - { address: your-host, user: root }

# Bundled Caddy fronts every `domain:`-tagged service with HTTPS.
# `email:` is sent to Let's Encrypt for ACME registration / expiry
# warnings — use a real address you actually read.
proxy:
  email: you@example.com           # ← REPLACE BEFORE DEPLOY

secrets:
  provider: age
  recipients:
    - age1…                       # generated by `yoink init`

services:
  - name: my-app
    image: my-app
    tag: latest
    # Public domain. Set DNS to point at the host's IP BEFORE
    # deploying — Let's Encrypt validates against the live IP.
    domain: app.example.com
    build:
      context: .
      # Cross-build to amd64 when developing on Apple Silicon.
      # Drop this if your laptop matches the host's arch.
      extra_args: ["--platform", "linux/amd64"]
    depends_on: [postgres, redis]
    env:
      POSTGRES_HOST: postgres
      POSTGRES_DB: app
      POSTGRES_USER: app
      REDIS_HOST: redis
    env_from_secrets:
      POSTGRES_PASSWORD: POSTGRES_PASSWORD
    run:
      port: 3000
      healthcheck_path: /

include:
  - "services/*.yaml"
```

Run `yoink validate` to confirm the config parses, then `yoink doctor` for a deploy-readiness check. Doctor surfaces unreachable hosts, unresolved domains, and placeholder emails before you spend time on a deploy that would fail.

The `domain:` line opts the app into yoink's bundled Caddy. Caddy will request a Let's Encrypt cert on first deploy — if your DNS isn't pointed at the host yet, you'll get a self-signed fallback and can re-deploy after fixing the record.

> **DNS is your responsibility.** Point an A record (and AAAA if you serve IPv6) for `app.example.com` at the host's public IP. yoink doesn't touch DNS — and Let's Encrypt won't issue without a working HTTP-01 path. Verify with `dig +short A app.example.com` before the deploy.

## Step 6: deploy

```sh
yoink up --service postgres --service redis    # pass 1: accessories
yoink up --build --no-registry --service my-app # pass 2: app, local build → unregistry push
```

Pass 1 pulls postgres + redis from Docker Hub, starts them on the `yoink` network. Pass 2 builds the app image locally, ships it via [unregistry](https://github.com/psviderski/unregistry) over SSH (no registry account required), starts the container, runs the healthcheck, then auto-injects a Caddy proxy that fronts `app.example.com` with HTTPS.

> **Why two passes?** With a single `yoink up --build --no-registry`, yoink's image-prefetch step still tries to pull every image — including the locally-built app, which 404s on Hub. Scoping with `--service` avoids the prefetch for accessories that don't need updating. (See [issue tracking the prefetch fix](#).)

The first deploy of the app is the slow one — pulling Docker Hub's `node:22-alpine` and the npm install. Re-deploys are fast: unregistry only ships changed layers (typically tens of KB for a code edit).

## Verify

Open `https://app.example.com/` (or `https://<host-ip>/` to bypass DNS). The index page renders the `DbStatus` JSON the loader fetched — both `postgres.ok` and `redis.ok` should be `true`, with the postgres version and the redis round-trip value visible.

If both report `ok: false` with `EAI_AGAIN`, the network alias didn't take — `yoink add` (versions before the fix that landed network_aliases in the templates) used to omit them. Update to a current `yoink add postgres` / `yoink add redis` and re-deploy.

## Re-deploys

Edit a route, save, deploy:

```sh
yoink up --build --no-registry --service my-app
```

~15 seconds for a typical code change. unregistry's layer dedup means only the rebuilt application layer ships across SSH; Node base images, the npm install layer, and the build dependencies stay cached on the host.

For tag-stamped deploys (so `yoink history` shows commits, not a string of `latest`s):

```sh
yoink up --build --no-registry --here --service my-app
```

`--here` substitutes `git rev-parse --short HEAD` for the tag.

## Real HTTPS, no certs to manage

The bundled Caddy handles ACME automatically. First deploy after DNS is correct: cert issuance happens in the background, takes ~10 seconds, persists across redeploys (sealed under yoink's keys). Renewals are automatic. The `proxy.email:` you set is what Let's Encrypt uses for expiry warnings.

If your DNS is on Cloudflare (or any other provider that proxies HTTP and rewrites the IP), see [Cloudflare origin certs](/docs/recipes/cloudflare-origin-certs) for the alternative pattern that doesn't require an LE handshake from the operator's host.

## What just happened

Five files (`Dockerfile`, `.dockerignore`, `src/lib/db.server.ts`, the modified `src/routes/index.tsx`, the edited `yoink.yaml`) plus three yoink commands (`init`, two `add`s, `up`). The result:

- Three containers on one host: postgres, redis, your app.
- Caddy auto-injected to front the app with HTTPS.
- Random `POSTGRES_PASSWORD` generated, sealed into `secrets.age`, and committed to the repo. Decryption needs the age key in `~/.config/yoink/keys/`.
- All inter-service traffic on a private docker network — neither postgres nor redis is exposed publicly.
- Spec-hash drift detection on every container, so a manual `docker exec` in production shows up in `yoink status` next time you check.

For the full `yoink up` flag list, see [first deploy](/docs/start/first-deploy) and the [CLI reference](/docs/reference/cli).

## Multiple projects on one host

Each `yoink up` against a config with any `domain:` service auto-injects a Caddy proxy that binds host ports 80 and 443. **Two separate `yoink.yaml`s on the same host fight over those ports** — and the second deploy wins, clobbering the first config's routes.

The pattern that works is "one yoink.yaml per host, multiple service fragments." The host's central yaml aggregates services from each project repo via the `include:` mechanism:

```yaml
# host-prod/yoink.yaml — the canonical config for `your-host`
hosts:
  - { address: your-host, user: root }
proxy:
  email: you@example.com
secrets:
  provider: age
  recipients: [age1…]

include:
  - "services/*.yaml"
  - "../project-a/services/*.yaml"   # mount each project's services
  - "../project-b/services/*.yaml"   # alongside its own repo
```

Each project's repo keeps its `services/<project>.yaml` fragment alongside its code; the host config aggregates them. Two services pointing at different domains get routed by the same Caddy. Two services sharing one domain need `path_prefix:` to disambiguate (see the [config reference](/docs/reference/config) for the routing rules).

`yoink up` from the host config sees every project's services as one cluster and updates Caddy with all routes at once. Per-project deploys are still possible — `yoink up --service project-a-app` from inside the host config — they just need to run from the directory with the central `yoink.yaml`.

Coordination cost: every project's CI runner needs the host config's age identity (or the per-project services need to ship their own `provider: command` entry that the host config inherits). Manageable for a few projects per host; switch to per-host yoink configs (different hosts, different proxies) once you outgrow the single-Caddy bottleneck.
