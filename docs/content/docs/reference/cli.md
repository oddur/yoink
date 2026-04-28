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
yoink init [HOST]                        generate a starter yoink.yaml in cwd. Detects
                                         Dockerfile (EXPOSE/USER/HEALTHCHECK) + git remote
                                         + ~/.ssh/config and writes a validated config
                                         with zero prompts. HOST optional when ssh config
                                         supplies a non-wildcard Host entry.
  --force                                overwrite an existing yoink.yaml
  --interactive                          prompt for every field instead of inferring
                                         (defaults match the inferences)
  --service <NAME>                       override the inferred service name
  --port <N>                             override inferred port (Dockerfile EXPOSE or 8080)
  --no-port                              skip port + healthcheck (no HTTP surface)
  --image <PATH>                         override the inferred image reference

yoink preflight                          verify Docker is reachable on each configured host

yoink up                                 reconcile every service to its desired spec
  --service <NAME>                       restrict to a subset (repeatable)
  --tag <[name=]value>                   override a service's tag (repeatable; bare form
                                         applies to every selected service)
  --here                                 use `git rev-parse --short HEAD` as the tag for
                                         every selected service. Conflicts with --tag.
  --plan                                 friendlier alias for --dry-run; mirrors
                                         `terraform plan` (read-only)
  --dry-run                              print the planned actions and exit (read-only).
                                         Output shows per-service +/-/~ diffs of env keys
                                         and label keys when --format text (default).
  --format <text|markdown|json>          dry-run output format
  --watch                                re-reconcile whenever the config file (or include
                                         fragment) changes on disk; polls every 2s. Ctrl-C
                                         exits. Cannot combine with --plan/--dry-run.
  --no-registry                          stream local image to host (no docker pull);
                                         pair with --build for the indie one-shot
  --build                                docker build any service with a `build:` block first
  --force                                redeploy even when at-spec; recovery gesture for
                                         drifted proxy state. Pair with --service to scope.
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

yoink logs <SERVICE>                     stream or tail logs. With multiple replicas and
                                         no --host, multiplexes across all of them with
                                         [host/container] line prefixes.
  --host <ADDRESS>                       pin to a specific replica
  --follow                               keep streaming as new lines arrive
  --tail <N>                             show the last N lines

yoink exec <SERVICE> -- <CMD> <ARGS>...  one-shot command inside a running container
yoink shell <SERVICE>                    interactive PTY shell (`bash` if present, else
                                         `sh`). With multiple replicas and no --host,
                                         picks the first healthy one and prints which.
                                         Aliased as `yoink ssh <SERVICE>`.
yoink ssh <SERVICE>                      alias for `yoink shell`
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

yoink secrets key generate               generate an age keypair. Prints both
                                         the IDENTITY (private, AGE-SECRET-KEY-1…)
                                         and matching RECIPIENT (public, age1…) —
                                         identity goes in your secret manager,
                                         recipient gets committed to yoink.yaml.
  --out <PATH>                           write the identity to PATH (mode 0600)
                                         instead of stdout. Make sure PATH is
                                         gitignored. The recipient still prints
                                         to stdout.
  --force                                overwrite an existing identity at --out
yoink secrets key public                 re-derive the recipient from whichever
                                         identity yoink would use right now
                                         (sanity-check vs yoink.yaml)
yoink secrets edit                       decrypt the configured sealed file into
                                         $EDITOR, re-seal on save (path comes from
                                         `secrets.file:`; defaults to secrets.age)
yoink secrets show [--reveal]            print KEY=value (values masked unless --reveal).
                                         Refuses --reveal in CI ($CI set) unless
                                         YOINK_ALLOW_REVEAL_IN_CI=1.
yoink secrets seal --in <PATH>           seal a plaintext dotenv (or read from stdin)
  --out <PATH>                           override the output path (defaults to
                                         the configured `secrets.file:`)
yoink secrets rotate                     generate a new keypair and re-seal under
                                         [existing recipients + new recipient];
                                         prints the new identity for pasting into
                                         your CI / secret manager

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
yoink up --here                                             # shorthand for "every service at HEAD"
yoink up --service api --here                               # one service at HEAD
```

`--here` resolves the current `git rev-parse --short HEAD` and applies it as a per-service override — equivalent to typing the SHA out for every service. Conflicts with `--tag` (use one or the other).

## Force redeploy (`--force`)

Bypasses the at-spec early-return so every selected service runs the full prepare → finalize loop, including the Caddy admin push for routed services. The recovery gesture for cases where proxy-side state has drifted from container reality — e.g. Caddy's upstream pool still references containers that were swapped out in a prior reconcile. Pair with `--service <name>` to limit blast radius:

```sh
yoink up --service yoink-proxy --force          # re-push Caddy admin config
yoink up --service api --force                  # re-create + healthcheck all api replicas
```

Tradeoff: for multi-replica services the prep phase removes the old replicas before finalize starts the new ones, so there's a brief service-level downtime window (≈ healthcheck timeout). Use sparingly. Routine deploys never need this — drift detection + the rolling-swap loop handle the normal case automatically.

## Watch mode (`--watch`)

`yoink up --watch` runs an initial reconcile, then keeps polling the config file every 2 seconds and re-reconciles on every change. Pairs with `--build --no-registry` for the edit-save-deploy inner loop:

```sh
yoink up --watch --build --no-registry --service my-tool
# edit Dockerfile / yoink.yaml → save → yoink rebuilds and ships
# Ctrl-C to exit
```

The 2 s cadence matches the TUI's reload tick. Reconcile errors are reported to stderr but don't abort the loop — fix the config and the next save kicks off another attempt.

## Dynamic shell completion

`yoink completions <shell>` emits static completions (subcommand names, flag names, value enums). For *dynamic* values — service names, host addresses, config file paths — yoink ships a hidden `__complete` helper that prints one value per line:

| Subcommand | Source | Needs config loaded? |
|---|---|---|
| `yoink __complete services` | service names from the current config | yes |
| `yoink __complete hosts` | host addresses from the current config | yes |
| `yoink __complete configs` | `yoink.yaml` / `*.yoink.yaml` files anywhere under cwd, ranked newest-first | no |

The walker behind `__complete configs` skips the usual noise directories (`.git`, `target`, `node_modules`, `.venv`, `dist`, `build`, …) and caps depth at 8, so a `<TAB>` press completes in well under 100 ms even on a large repo.

Bash:

```bash
# ~/.bashrc (alongside `source <(yoink completions bash)`)
_yoink_dynamic() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local prev="${COMP_WORDS[COMP_CWORD-1]}"
    # Forward any -c / --config that's already on the line so that
    # `yoink -c staging.yoink.yaml shell <TAB>` lists *staging's*
    # services, not the default config's.
    local fwd=()
    for ((i=1; i<COMP_CWORD; i++)); do
        case "${COMP_WORDS[i]}" in
            -c|--config)
                fwd=(-c "${COMP_WORDS[i+1]}")
                ;;
        esac
    done
    case "$prev" in
        -c|--config)
            COMPREPLY=( $(compgen -W "$(yoink __complete configs 2>/dev/null)" -- "$cur") )
            return 0
            ;;
        --service|-s|shell|ssh|exec|logs|restart|kill|debug|version|history|rollback|pull|diff)
            COMPREPLY=( $(compgen -W "$(yoink "${fwd[@]}" __complete services 2>/dev/null)" -- "$cur") )
            return 0
            ;;
        --host)
            COMPREPLY=( $(compgen -W "$(yoink "${fwd[@]}" __complete hosts 2>/dev/null)" -- "$cur") )
            return 0
            ;;
    esac
}
complete -F _yoink_dynamic -o default yoink
```

Zsh: drop the `compdef` snippet from `yoink completions zsh` into your `fpath`, then layer a wrapper that calls `yoink __complete <kind>` for the values you want described. The helper itself just prints `\n`-separated paths/names — wire it into whatever completion shape your shell prefers.

After sourcing the snippet, every `<TAB>` works the way you'd hope:

```sh
yoink -c <TAB>                    # all yoink configs in this repo
yoink shell <TAB>                 # services from the default config
yoink -c staging.yoink.yaml shell <TAB>   # services from staging
```
