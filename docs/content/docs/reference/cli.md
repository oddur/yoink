---
title: CLI
weight: 6
---

## Global flags

Every subcommand accepts:

```
-c, --config <PATH>   Path to the yoink.yaml config file (default: ./yoink.yaml)
-v, --verbose         Increase log verbosity (-v info, -vv debug, -vvv trace)
-h, --help            Per-subcommand help
-V, --version         Print version
```

## Subcommand reference

```
yoink preflight                          verify Docker is reachable on each configured host

yoink up                                 reconcile every service to its desired spec
  --service <NAME>                       restrict to a subset (repeatable)
  --tag <[name=]value>                   override a service's tag (repeatable; bare form
                                         applies to every selected service)
  --dry-run                              print the planned actions and exit (read-only)
  --format <text|markdown|json>          dry-run output format
  --no-registry                          stream local image to host (no docker pull);
                                         pair with --build for the indie one-shot
  --build                                docker build any service with a `build:` block first
  --allow-dirty                          allow operating with a dirty git working tree

yoink build [<SERVICE>]                  docker build the named service (or every service
                                         with a build: block when no SERVICE)
  --tag <[name=]value>                   override the resolved tag
  --no-cache                             forward to docker build
  --push                                 docker push <image>:<tag> after a successful build

yoink status                             snapshot of what's running where
  --json                                 emit a structured StatusReport for CI assertions

yoink rollback <SERVICE>                 redeploy the previous spec_hash
  --tag <value>                          skip discovery, pin to a specific past tag

yoink history <SERVICE>                  list past deploys (when, version, state, deployed-by)
  --limit <N>                            cap the row count

yoink prune                              remove yoink-managed containers no longer in config
  --dry-run                              show what would be removed

yoink diff <SERVICE>                     show what would change between running and target
  --tag <value>                          diff against a specific image tag

yoink dump                               dense JSON of everything yoink can observe — config,
                                         per-host docker info, every yoink container with
                                         inspect + stats + log tail + drift status. Pipe into
                                         an LLM/agent for diagnosis. Secret-ish env values
                                         are redacted.

yoink logs <SERVICE>                     stream or tail logs
  --follow                               keep streaming as new lines arrive
  --tail <N>                             show the last N lines

yoink exec <SERVICE> -- <CMD> <ARGS>...  one-shot command inside a running container
yoink shell <SERVICE>                    interactive PTY shell (`bash` if present, else `sh`)
yoink debug <SERVICE>                    alpine debug sidecar in target's pid+net ns (for
                                         distroless / shell-less images)
yoink restart <SERVICE>                  bounce the container without re-deploying
yoink kill <SERVICE> [--yes]             SIGKILL a container; no graceful drain
yoink pull <SERVICE> [--tag <value>]     pre-warm an image without deploying

yoink networks                           list docker networks across hosts
yoink volumes                            list docker volumes across hosts
yoink top                                htop-style CPU/mem snapshot per running container
yoink version <SERVICE>                  print currently-running tag(s) per replica

yoink validate                           lint the config; optionally ping each docker daemon
yoink lock                               inspect / release the per-host deploy lock (useful
                                         after a crashed deploy left a sentinel container)
yoink completions <shell>                generate shell completions (bash/zsh/fish/...)

yoink secrets keygen                     generate an age identity at ~/.config/yoink/age.key
                                         and print the public recipient
  --out <PATH>                           override the destination
  --force                                overwrite an existing identity
yoink secrets edit                       decrypt secrets.age into $EDITOR, re-seal on save
yoink secrets show [--reveal]            print KEY=value (values masked unless --reveal)
yoink secrets seal --in <PATH>           seal a plaintext dotenv (or read from stdin)
  --out <PATH>                           override the output path

yoink tui                                interactive ratatui dashboard (see TUI page)
```

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
