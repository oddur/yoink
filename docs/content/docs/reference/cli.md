---
title: CLI
description: Every subcommand, flag, and tag-resolution rule, including diagnostic commands like yoink doctor and yoink status.
weight: 6
---

## Global flags

Every subcommand accepts:

```
-c, --config <PATH>   Path to the yoink.yaml config file (default: ./yoink.yaml)
-v, --verbose         Increase log verbosity (-v info, -vv debug, -vvv trace).
                      Repeatable.
-q, --quiet           Suppress informational stderr (-q most, -qq all but errors).
                      Repeatable. Doesn't affect stdout; primary results stay
                      readable.
    --color <COLOR>   When to colour output: auto (default; detects TTY) /
                      always / never. Honours NO_COLOR regardless.
-h, --help            Per-subcommand help
-V, --version         Print version
```

Env vars yoink reads: `YOINK_AGE_KEY` / `YOINK_AGE_KEY_FILE` (sealed-secrets
identity), `YOINK_ALLOW_REVEAL_IN_CI` (override `secrets show --reveal`'s CI
guard), `EDITOR` / `VISUAL` (`secrets edit`), `PAGER` (long output),
`NO_COLOR` (disable colour), `CI` (triggers the reveal guard above), `RUST_LOG`
(fine-grained per-module logging; overrides `-v`).

## Subcommand reference

```
yoink init [HOST]                        generate yoink.yaml + an age identity for sealed
                                         secrets. Detects Dockerfile / git remote /
                                         ~/.ssh/config and writes a validated config with
                                         zero prompts. The identity lands at
                                         ~/.config/yoink/keys/<recipient>.key (mode 0600);
                                         a matching `secrets:` block goes into the yaml.
                                         HOST optional when ssh config has a non-wildcard
                                         entry.
  --force                                overwrite an existing yoink.yaml
  --interactive                          prompt for every field instead of inferring
                                         (defaults match the inferences)
  --service <NAME>                       override the inferred service name
  --port <N>                             override inferred port (Dockerfile EXPOSE or 8080)
  --no-port                              skip port + healthcheck (no HTTP surface)
  --image <PATH>                         override the inferred image reference
  --no-secrets                           skip generating an age identity (use when bringing
                                         your own key, or when the project will use
                                         `provider: command` for secrets)

yoink add [REF]                          drop a vetted template into your repo: fetch from
                                         GitHub, run a wizard for variables, seal generated
                                         secrets, extend yoink.yaml's `include:`. Bare REF
                                         (`postgres`) for the bundled set; `gh:owner/repo/path`
                                         for 3rd-party. Omit REF for the interactive picker.
                                         See the templates guide.
  --from-path <PATH>                     local directory as the template source instead of
                                         GitHub (template-author iteration). PATH must
                                         contain a template.yaml.
  --refresh                              re-resolve a branch/tag ref to the latest SHA,
                                         bypassing the local cache mapping. Pinned-SHA
                                         refs are unaffected.
  --var <KEY=VALUE>                      manifest variable override (repeatable)
  --up                                   run `yoink up` after the fragment is in place
                                         (auto-yes for `kind: app` templates)
  --yes                                  skip every confirmation prompt; required in CI

yoink preflight                          verify Docker is reachable on each configured host
  --wait <DURATION>                      poll until each host's docker daemon responds
                                         (or the budget lapses); useful right after
                                         provisioning a fresh host. e.g. `--wait 90s`

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
  --no-registry                          force local-only mode: ship every selected service's
                                         image from the operator's docker daemon to each host,
                                         skipping all registry pulls. Usually unnecessary —
                                         services with a `build:` block are shipped from local
                                         automatically; this widens the path to non-build
                                         services too (offline / airgapped / locally-modified
                                         public image)
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

yoink audit log                          merge operator-side log
                                         ($XDG_STATE_HOME/yoink/audit/events.jsonl) with each
                                         host's /var/lib/yoink/audit/events.jsonl, dedupe on
                                         `event_id`, sort newest-first
  --host <ADDR>                          restrict the host fetch to one host
  --service <NAME>                       restrict to one service
  --deploy-id <ID>                       only events from this run (prefix-match)
  --event <NAME>                         filter by event name (repeatable)
  --origin operator|host                 only one side of the merge
  --since <DURATION>                     window relative to now (default 7d)
  --limit <N>                            cap the row count (default 100)
  --format text|json                     `json` emits raw JSONL — pipes cleanly into jq
yoink audit run <DEPLOY_ID>              every event for one run, ordered, across all hosts.
                                         Useful for reconstructing what one `yoink up` did.
yoink audit gc [--keep <DURATION>]       remove rotated audit files older than --keep
                                         (default 90d). Active events.jsonl is never touched.
  --host <ADDR>                          restrict to one host
  --dry-run                              preview what would be removed

yoink prune                              remove yoink-managed containers no longer in config
  --dry-run                              show what would be removed

yoink diff <SERVICE>                     show what would change between running and target
  --tag <value>                          diff against a specific image tag

yoink dump                               dense JSON of everything yoink can observe: config,
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
yoink pf <SERVICE> [[LOCAL:]CPORT]       port-forward a container port to the laptop. Auto:
                                         uses the host's `docker-proxy` listener when the
                                         service has a matching `publish:` entry, else
                                         spawns an `alpine/socat` sidecar that joins the
                                         service's docker network and forwards. Holds
                                         until SIGINT; sidecar is force-removed on exit.
  --mode <auto|published|sidecar>        path selection. `auto` default; `published` errors
                                         when no publish matches; `sidecar` always spawns a
                                         sidecar even if a publish would have worked.
  -o, --open                             open the URL in the system browser when ready
  --scheme <SCHEME>                      override the URL scheme used by --open / printed
                                         link (auto: 80/3000/5050/… → http, 443 → https)
  --json                                 first stdout line is `{local_port, mode, url, …}`,
                                         then keep tunneling. Useful for scripts that
                                         need to read the assigned port.
  -r, --replica <N>                      replica index (defaults to 0)
  --host <ADDRESS>                       pin to a specific host on multi-host configs
yoink restart <SERVICE>                  bounce the container without re-deploying
yoink kill <SERVICE> [--yes]             SIGKILL a container; no graceful drain
yoink pull <SERVICE> [--tag <value>]     pre-warm an image without deploying

yoink networks                           list docker networks across hosts
yoink volumes                            list docker volumes across hosts
yoink top                                htop-style CPU/mem snapshot per running container
yoink version <SERVICE>                  print currently-running tag(s) per replica

yoink validate                           lint the config; optionally ping each docker daemon
  --check-hosts                          also test the ssh+docker connection to every host
                                         (CI gate for "the deploy will actually go through")
yoink doctor                             diagnose deploy-blockers (host reachability, age
                                         identity, DNS for `domain:` services, arch alignment,
                                         common config friction). Exits non-zero on Errors.
  --json                                 output as JSON for piping / agent consumption.
                                         Each entry: `{severity, category, title, detail, fix}`.

yoink proxy-render                       print the Caddy admin-API JSON yoink would push for
                                         the current config. Read-only; no host connection
                                         required (uses service-name fallbacks instead of
                                         live container lookups). Pipe into `caddy adapt` or
                                         a debug Caddy's `/load` for schema validation.
yoink proxy-dockerfile                   print the synthesized xcaddy Dockerfile for the
                                         current `proxy.xcaddy:` config. No docker calls;
                                         useful for code review or pinning a Dockerfile in CI.

yoink lock <COMMAND>                     inspect / release the per-host deploy lock
  status                                 print holder + age of the deploy lock on each host
  release                                force-remove the deploy lock sentinel on each host.
                                         Use after a crashed deploy left a sentinel running;
                                         only safe when no operator is actually deploying.

yoink completions <shell>                generate shell completions (bash/zsh/fish/...)

yoink secrets key generate               generate an age keypair. By default the
                                         IDENTITY (private) is saved to
                                         ~/.config/yoink/keys/<recipient>.key
                                         (mode 0600) and yoink finds it
                                         automatically next time. The matching
                                         RECIPIENT (public, age1…) is printed
                                         for adding to yoink.yaml.
  --out <PATH>                           write the identity to PATH (mode 0600)
                                         instead of the default keys dir.
  --print                                print the secret to stdout (for piping
                                         into a CI secret store, e.g.
                                         `… --print | gh secret set YOINK_AGE_KEY`).
                                         Recipient/header/notes go to stderr so
                                         the pipe captures only the key bytes.
  --force                                overwrite an existing identity at the
                                         destination
yoink secrets key public                 re-derive the recipient from whichever
                                         identity yoink would use right now
                                         (sanity-check vs yoink.yaml)
yoink secrets edit                       decrypt the configured sealed file into
                                         $EDITOR, re-seal on save (path comes from
                                         `secrets.file:`; defaults to secrets.age)
yoink secrets show [--reveal]            print KEY=value (values masked unless --reveal).
                                         Refuses --reveal in CI ($CI set) unless
                                         YOINK_ALLOW_REVEAL_IN_CI=1.
yoink secrets env                        decrypt the bundle and print
                                         `export KEY='value'` lines for sourcing
                                         into a local dev shell, e.g.
                                         `source <(yoink secrets env)`. Values
                                         are POSIX single-quoted so `$`, quotes,
                                         and newlines pass through literally.
                                         Same CI guard as `show --reveal`.
  --no-export                            emit bare `KEY='value'` lines (dotenv
                                         style) instead of `export KEY='value'`.
  --profile <NAME>                       apply a `secrets.profiles` recipe from yoink.yaml:
                                         filter to its `include` keys, rename per its
                                         `rename` map, prepend `unset 'KEY'` lines for
                                         each `unset` entry. When `$GITHUB_ENV` is set,
                                         auto-prepend `::add-mask::` lines for every
                                         revealed value. See the secrets profiles section
                                         in the configuration reference.
yoink secrets seal --in <PATH>           seal a plaintext dotenv (or read from stdin)
  --as KEY=value                         set one key directly; --as KEY=@PATH reads
                                         the value from a file. Repeatable.
  --out <PATH>                           override the output path (defaults to
                                         the configured `secrets.file:`)
  --replace                              wholesale-rewrite the bundle (default is
                                         merge); prompts on key drops, --yes skips
                                         the prompt
yoink secrets rotate                     generate a new keypair and re-seal under
                                         [existing recipients + new recipient];
                                         prints the new identity for pasting into
                                         your CI / secret manager

yoink tui                                interactive terminal dashboard (see TUI page)
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

`--here` resolves the current `git rev-parse --short HEAD` and applies it as a per-service override, equivalent to typing the SHA out for every service. Conflicts with `--tag` (use one or the other).

## Force redeploy (`--force`)

Bypasses the at-spec early-return so every selected service runs the full prepare → finalize loop, including the Caddy admin push for routed services. The recovery gesture for cases where proxy-side state has drifted from container reality; e.g. Caddy's upstream pool still references containers that were swapped out in a prior reconcile. Pair with `--service <name>` to limit blast radius:

```sh
yoink up --service yoink-proxy --force          # re-push Caddy admin config
yoink up --service api --force                  # re-create + healthcheck all api replicas
```

Tradeoff: for multi-replica services the prep phase removes the old replicas before finalize starts the new ones, so there's a brief service-level downtime window (≈ healthcheck timeout). Use sparingly. Routine deploys never need this; drift detection + the rolling-swap loop handle the normal case automatically.

## Watch mode (`--watch`)

`yoink up --watch` runs an initial reconcile, then keeps polling the config file every 2 seconds and re-reconciles on every change. Pairs with `--build` for the edit-save-deploy inner loop:

```sh
yoink up --watch --build --service my-tool
# edit Dockerfile / yoink.yaml → save → yoink rebuilds and ships
# Ctrl-C to exit
```

The 2 s cadence matches the TUI's reload tick. Reconcile errors are reported to stderr but don't abort the loop; fix the config and the next save kicks off another attempt.

## Boolean-flag conventions

yoink uses three deliberately distinct shapes for boolean flags:

| Shape | Meaning | Examples |
|---|---|---|
| `--<verb>` | Enable an additive behavior. Off by default. | `--build`, `--watch`, `--push`, `--json`, `--force` |
| `--no-<thing>` | Suppress a default-on behavior. | `--no-registry` (force local-only mode, ships every service's image from local instead of pulling), `--no-port` (skip port/healthcheck inference), `--no-secrets` (skip identity bootstrap), `--no-cache` (skip docker build cache) |
| `--allow-<safety>` | Override a safety guard. The default refuses; `--allow-X` opts in. | `--allow-dirty` (deploy with a dirty git tree) |

Same rules for new flags: `--allow-X` only when there's a guard to bypass; `--no-X` only when the default is on; otherwise plain `--<verb>`. Avoid `--skip-X` / `--without-X`; they overlap with `--no-X`.

`--yes` is the standard non-interactive override for confirmation prompts; it does not enable destructive behavior on its own (the action is what enables it).

## Dynamic shell completion

`yoink completions <shell>` emits static completions (subcommand names, flag names, value enums). For *dynamic* values (service names, host addresses, config file paths), yoink ships a hidden `__complete` helper that prints one value per line:

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

Zsh: drop the `compdef` snippet from `yoink completions zsh` into your `fpath`, then layer a wrapper that calls `yoink __complete <kind>` for the values you want described. The helper itself just prints `\n`-separated paths/names; wire it into whatever completion shape your shell prefers.

After sourcing the snippet, every `<TAB>` works the way you'd hope:

```sh
yoink -c <TAB>                    # all yoink configs in this repo
yoink shell <TAB>                 # services from the default config
yoink -c staging.yoink.yaml shell <TAB>   # services from staging
```

## See also

- [Configuration reference](/docs/reference/config) — schema for `yoink.yaml`.
- [TUI reference](/docs/reference/tui) — keybinds for the terminal dashboard.
- [Troubleshooting](/docs/troubleshooting) — error messages, likely causes, fixes.
