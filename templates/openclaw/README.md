# openclaw template

Placeholder "app" template demonstrating `yoink add <name>` for a
complete deployable application (as opposed to an accessory like
`postgres` or `redis`).

This template is intentionally generic. After running `yoink add openclaw`,
edit `services/<name>.yaml` to point at your real image and wire env
vars / secrets to your app's needs.

## What you get

- A service entry behind yoink's bundled Caddy with HTTPS (when `domain`
  is set).
- One sealed secret (`<NAME>_SECRET_KEY`) — useful as a session signing
  key, or replace with whatever your app needs.
- An HTTP healthcheck on `/health` (rolling-swap-gated). Change the path
  if your app exposes its healthcheck elsewhere.

## What to customise

- `image:` — point at your image; remove `tag:` if you'll pin per-deploy
  with `--tag`.
- `env:` and `env_from_secrets:` — your app's runtime config.
- `depends_on:` — list any accessories (`postgres`, `redis`, …) you've
  also added with `yoink add`.

## Why "openclaw"?

It's a placeholder name. Rename the directory, the manifest's `name:`,
and the variable defaults to whatever your app is actually called, then
fork to your own templates repo and use `gh:youruser/yourrepo/yourapp`
as the ref.
