---
title: Pre-deploy hooks
description: Why yoink runs infra glue as a pre-deploy hook, how the subprocess and container flavors differ, and how hooks relate to drift detection.
---

Some services cannot deploy until something outside the container is in place: DNS records, an origin cert, a managed database, an IAM role. A manual step before every deploy is easy to forget. The same step inside CI doubles the surface of the deploy job.

A pre-deploy hook runs a command or a container before the service swap, with access to the same sealed bundle. With a [`secrets_profile`](/docs/reference/config#secrets-profiles), one block names the env shape that the tool needs.

## Two hook flavors

| flavor | when | shape | use for |
|---|---|---|---|
| **subprocess** | yoink runs the binary directly on your computer (or on the CI runner). | `cmd: [...]`, no `image:`, no `tag:` | Tools that are already on your PATH: `terraform`, `dbmate`, `alembic`, `gcloud`, `stripe`. |
| **container** | yoink pulls an image and runs the command inside Docker on the first host in `hosts:`. The hook joins the first network in `deploy.networks`. A hook of a service runs on the first host of that service. It joins the first network in `deploy.networks` that the service is on. | `image:` + `tag:` + `cmd:` | Isolation, portable runtimes, and a pinned tool version that is not on the computer of every operator. |

The two flavors share `secrets:`, `env_from_secrets:`, and `secrets_profile:`. A subprocess hook needs no Dockerfile and no registry round trip.

## When to use a container hook instead

The subprocess flavor is the correct default. The container flavor fits these cases:

- The binary is not on the computer of every operator, and you do not want to pin it with devbox or homebrew.
- You need isolation from your filesystem (a write-only sandbox, a scratch filesystem).
- The hook runs an LLM agent, a migration tool, or an audit script that you want to pin to a specific image hash for reproducibility.

## Drift and rotation

A hook is not an input of [`spec_hash`](/docs/explanation/architecture#drift-detection). A new key in the profile, or a rotated value behind a key, does not schedule a redeploy by itself.

yoink resolves the env of a hook from the sealed bundle each time the hook runs. A top-level hook runs on every `yoink up`. A hook of a service runs only when that service deploys. Thus the profile is the single source of truth for the credentials that the hook reads.

If the service reads the same secret, the rotated value changes the `spec_hash` of the service. Then one deploy runs the hook and swaps the container, and the two use the same value.

## Beyond Terraform

Hooks are not specific to Terraform. Any deploy-time glue fits the same pattern: an alembic migration runner, a Stripe-pricing reconciler, a `gcloud` IAM apply, a `dbmate` schema migration. The one condition is that the tool reads its credentials from env vars. Then a profile and a subprocess hook connect the sealed bundle to the tool with one declarative block per consumer.

## See also

- [Run Terraform (or any infra glue) from a hook](/docs/how-to/terraform-from-hooks): the steps for a subprocess hook with a secrets profile.
- [Config reference: Hook](/docs/reference/config#hook): the keys of a hook entry.
- [Secrets profiles](/docs/reference/config#secrets-profiles): the schema for `include`, `rename` and `unset`.
- [Architecture: drift detection](/docs/explanation/architecture#drift-detection): how `spec_hash` finds a change.
