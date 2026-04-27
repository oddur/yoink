---
title: CLI
weight: 6
---

```
yoink preflight                          verify Docker is reachable on each host
yoink up                                 reconcile every service in dep order
  --service <name>                       restrict to a subset (repeatable)
  --tag <name=value>                     override a service's tag (repeatable)
  --dry-run                              print the planned actions and exit
  --format <text|markdown|json>          dry-run output format
  --no-registry                          stream local image to host (no docker pull)
  --build                                docker build any service with a build: block first
  --allow-dirty                          allow operating with a dirty git tree

yoink build [<service>]                  docker build the named service (or every service with a build: block)
  --tag <name=value>                     override the resolved tag
  --no-cache                             forward to docker build
  --push                                 docker push after build

yoink status                             show running containers across hosts
  --json                                 emit a structured StatusReport for CI assertions

yoink rollback <service>                 redeploy the previous spec_hash
  --tag <value>                          pin to a specific past tag

yoink prune                              remove yoink-managed containers no longer in config
  --dry-run                              show what would be removed

yoink history <service>                  list past deploys (when, version, state, deployed-by)
  --limit <N>                            cap the row count

yoink exec <service> <cmd>...            run a one-shot command in a running container
yoink shell <service>                    drop into a /bin/sh PTY in a running container
yoink restart <service>                  stop+start the running container
yoink kill <service>                     SIGKILL the running container
yoink pull <service>                     pre-pull the image without deploying
yoink networks                           list docker networks per host
yoink volumes                            list docker volumes per host
yoink top                                live CPU/memory per running container
yoink tui                                k9s-style dashboard
```

Run any subcommand with `--help` for the full flag list and inline docs.

## Resolving tags

Yoink resolves a service's tag in this order:

1. `--tag name=value` on the CLI overrides everything
2. A bare `--tag value` (no `name=`) applies to every selected service
3. The `tag:` field in the service config

Services without an explicit tag (typical for `image: ghcr.io/you/api` where the right answer is "whatever CI just built") error if no `--tag` override is provided. Stable infrastructure (caddy, redis) keeps a literal `tag: 7-alpine` so `yoink up` works without arguments.

## Tag overrides

```sh
yoink up --tag api=$(git rev-parse HEAD) --tag web=$(git rev-parse HEAD)
yoink up --service api --tag $(git rev-parse HEAD)         # bare form, single service
```
