---
title: CLI
description: Every subcommand, flag, and tag-resolution rule, with the diagnostic commands yoink doctor and yoink status.
weight: 6
---

Precedence: CLI flags, then environment variables, then `yoink.yaml`, then defaults.

## Global flags

Every subcommand accepts these flags:

| Flag | Effect |
|---|---|
| `-c, --config <PATH>` | Path to the config file. Default: `yoink.yaml` in the current directory. |
| `-v, --verbose` | More log output: `-v` info, `-vv` debug, `-vvv` trace. The default level is warn. |
| `-q, --quiet` | Less stderr output: `-q` hides progress lines, `-qq` hides all but errors. Does not change stdout. |
| `--color <auto\|always\|never>` | When to color output. Default: `auto`. `NO_COLOR` disables color in all cases. |
| `--require-latest-config` | Stops with an error if the upstream branch has commits to the config file that are not in `HEAD`. Without the flag, yoink prints a warning. |
| `-h, --help` | Help for the subcommand. |
| `-V, --version` | Prints the version. |

yoink reads these environment variables:

| Variable | Purpose |
|---|---|
| `YOINK_AGE_KEY` / `YOINK_AGE_KEY_FILE` | The identity for sealed secrets: the raw key, or the path to a key file. |
| `YOINK_ALLOW_REVEAL_IN_CI` | Overrides the CI guard of `secrets show --reveal` and `secrets env`. Accepts `1`, `true`, `yes`, `on`. |
| `YOINK_REQUIRE_LATEST_CONFIG` | Same as `--require-latest-config`. Accepts `1`, `true`, `yes`, `on`. |
| `YOINK_NO_HL` | Stops the TUI from piping logs through `hl`. |
| `EDITOR` / `VISUAL` | The editor for `secrets edit`. |
| `PAGER` | The pager for long output. Default: `less -FIRX`. |
| `NO_COLOR` | Disables color. |
| `CI` | Starts the CI guard of `secrets show --reveal` and `secrets env`. The variables of common CI providers (for example `GITHUB_ACTIONS`, `GITLAB_CI`) also start it. |
| `RUST_LOG` | Sets the log level for each module. Overrides `-v`. |
| `XDG_STATE_HOME` | Parent directory of the TUI log file and the operator-side audit log. Default: `~/.local/state`. |

## Subcommand reference

`<SERVICE>` is a service name from the config. `--host <ADDRESS>` takes a host address from the config.

### Setup

| Command or flag | Effect |
|---|---|
| `yoink init [HOST]` | Writes a starter `yoink.yaml` and an age identity for sealed secrets, with no prompts. Reads the Dockerfile, the git remote, and `~/.ssh/config`. The identity goes to `~/.config/yoink/keys/<recipient>.key` (mode 0600). `HOST` is optional when `~/.ssh/config` has a non-wildcard entry. |
| `--force` | Overwrites an existing `yoink.yaml`. |
| `--interactive` | Prompts for every field. The defaults are the inferred values. |
| `--service <NAME>` | Overrides the inferred service name. |
| `--port <N>` | Overrides the inferred port (Dockerfile `EXPOSE`, or 8080). |
| `--no-port` | Writes no `port:` and no `healthcheck_path:` (for a service with no HTTP surface). |
| `--image <PATH>` | Overrides the inferred image reference. |
| `--no-secrets` | Does not generate an age identity. For your own key, or for `provider: command`. |
| `--create-ssh-key <NAME>` | Generates an ed25519 SSH keypair and seals the private key under `NAME`. The config refers to it through `ssh_key_secret:`. Conflicts with `--no-secrets`. |
| `--proxy-email <EMAIL>` | Writes `proxy.email:` into the config. |
| `yoink add [REF]` | Adds a template to your repo: fetches it, asks for its variables, seals generated secrets, and extends `include:` in `yoink.yaml`. `REF` is a bundled name (`postgres`), a pinned name (`postgres@<sha>`), or `gh:owner/repo[@ref]/path`. Without `REF`, yoink opens the picker. |
| `--from-path <PATH>` | Uses a local directory that contains a `template.yaml` as the source. Conflicts with `REF`. |
| `--refresh` | Resolves a branch or tag ref to the latest SHA again and ignores the local cache. Does not change refs that are pinned to a SHA. |
| `--var <KEY=VALUE>` | Sets a manifest variable. Repeatable. |
| `--up` | Runs `yoink up` after the fragment is in place. |
| `--yes` | Skips every confirmation prompt. Required in CI. |
| `yoink hosts add` | Writes a host fragment to `<dest-dir>/<name>.yaml`. If the new address collides with an existing host, yoink removes the fragment and stops with an error. |
| `--address <ADDR>` | Hostname or IP of the host. Conflicts with `--address-secret`. |
| `--address-secret <NAME>` | Name of the sealed secret that holds the address. |
| `--user <USER>` | SSH user. Default: `root`. |
| `--ssh-key-secret <NAME>` | Name of a sealed SSH private key that yoink uses for the connection. |
| `--name <NAME>` | File name of the fragment. Default: the address with `:` and `.` replaced by `-`. Required with `--address-secret`. |
| `--dest-dir <DIR>` | Directory of the fragment. Default: `hosts`. Also on `hosts remove`. |
| `yoink hosts list` | Prints one line for each host in the loaded config. |
| `yoink hosts remove <NAME>` | Removes `<dest-dir>/<NAME>.yaml`. Stops with an error if the file does not exist. |
| `yoink completions <shell>` | Prints shell completions (bash, zsh, fish, and more). |

### Deploy

| Command or flag | Effect |
|---|---|
| `yoink up` | Reconciles every service to its config. |
| `-s, --service <NAME>` | Limits the run to one service. Repeatable. |
| `--tag <[NAME=]TAG>` | Overrides the tag of a service. Repeatable. See [Resolving tags](#resolving-tags). |
| `--here` | Uses `git rev-parse --short HEAD` as the tag of every selected service. Conflicts with `--tag`. |
| `--dry-run` | Prints the planned actions and exits. Connects to each host, but changes nothing. |
| `--plan` | Alias of `--dry-run`. |
| `--format <text\|markdown\|json>` | Output format of the dry run. Default: `text`, which shows `+`/`-`/`~` diffs of env keys and label keys for each service. |
| `--watch` | Reconciles again when the config changes on disk. Polls every 2 s. Cannot combine with `--plan` or `--dry-run`. |
| `--build` | Runs `docker build` first for each selected service that has a `build:` block. Does not push. |
| `--no-registry` | Ships the image of every selected service from your local docker daemon and pulls nothing. Services with a `build:` block ship from local without this flag. |
| `--transport <auto\|unregistry\|tarball>` | How yoink ships local images. `unregistry` sends only the layers that the host does not have. `tarball` streams the full `docker save` output. `auto` (default) uses `unregistry`, then `tarball` on failure. |
| `--rebuild-proxy` | Builds the xcaddy proxy image again, also when the host has the `yoink-caddy:<hash>` tag. No effect without `proxy.xcaddy:`. |
| `--force` | Deploys again when the container is at spec. See [Force redeploy](#force-redeploy---force). |
| `--allow-dirty` | Permits a git working tree with uncommitted changes. Turns off the git SHA fallback for tags. |
| `yoink build` | Runs `docker build` for every service that has a `build:` block and tags the result `<image>:<tag>`. |
| `-s, --service <NAME>` | Limits the build to one service. Repeatable. A service without a `build:` block is an error. |
| `--tag <[NAME=]TAG>` | Overrides the tag, as for `up`. |
| `--no-cache` | Passes `--no-cache` to `docker build`. |
| `--push` | Runs `docker push <image>:<tag>` after the build. |
| `--allow-dirty` | Same as for `up`. |
| `yoink rollback` | Deploys the previous generation of each service again. The previous generation is the newest stopped container whose `spec_hash` is different from the running one. |
| `-s, --service <NAME>` | Rolls back one service only. |
| `--host <ADDRESS>` | Looks for the previous generation on this host only. |
| `--dry-run` | Prints the service, host, and previous tag, and deploys nothing. |
| `yoink pull <SERVICE>` | Pulls the image on the hosts and does not deploy. |
| `--tag <TAG>` | Overrides the tag. |
| `--host <ADDRESS>` | Pulls on one host only. |
| `yoink prune` | Removes yoink-managed containers whose service is not in the config, and stale exited containers. |
| `--dry-run` | Lists the containers and removes nothing. |
| `yoink preflight` | Makes sure that Docker is reachable on each host. |
| `--wait <DURATION>` | Polls each host until its docker daemon answers or the time is over, for example `--wait 90s`. |
| `yoink validate` | Lints the config. If the proxy is enabled and Docker is available locally, it also validates the generated Caddy config. |
| `--check-hosts` | Also runs the preflight probe against every host. |

### Inspect

| Command or flag | Effect |
|---|---|
| `yoink status` | Shows what runs on which host. |
| `--json` | Prints a structured `StatusReport`. |
| `yoink diff <SERVICE>` | Shows the changes between the running container and the target spec. |
| `--tag <TAG>` | Uses this tag for the target. |
| `yoink history <SERVICE>` | Lists the yoink-managed containers of the service (running and exited), newest first: host, container, version, state, deployed-by, when. |
| `--limit <N>` | Maximum number of rows. Default: 20. |
| `--format <text\|json>` | Output format. Default: `text`. |
| `yoink version <SERVICE>` | Prints the running tag of each replica. |
| `yoink logs <SERVICE>` | Prints the logs of the service. With more than one replica, each line has a `[host/container]` prefix. |
| `--host <ADDRESS>` | Limits the logs to the replicas on one host. |
| `-f, --follow` | Streams new lines until Ctrl-C. |
| `--tail <N>` | Number of history lines. Default: 50. |
| `yoink top` | Shows a CPU and memory snapshot of every running yoink-managed container, sorted by CPU. |
| `--limit <N>` | Maximum number of rows. Default: 30. |
| `--format <text\|json>` | Output format. Default: `text`. |
| `yoink networks` | Lists the docker networks on all hosts. `--host <ADDRESS>` limits the list to one host. |
| `yoink volumes` | Lists the docker volumes on all hosts. `--host <ADDRESS>` limits the list to one host. |
| `yoink dump` | Prints JSON of everything that yoink can observe: config, docker info of each host, and every yoink container with inspect data, stats, log tail, and drift status. Redacts env values that look like secrets. |
| `--log-tail <N>` | Log lines for each container. Default: 50. |
| `yoink doctor` | Finds deploy blockers: host reachability, age identity, DNS for `domain:` services, arch alignment, and common config errors. Exits non-zero on errors. |
| `--json` | Prints JSON. Each entry is `{severity, category, title, detail, fix}`. |
| `yoink tui` | Starts the terminal dashboard. See [TUI](/docs/reference/tui). |
| `--mode <MODE>` | The pane to open first: `dashboard` (default), `hosts`, `services`, `logs`, `resources`, `secrets`, `audit`. |
| `--mouse` | Enables mouse capture. |

### Containers

| Command or flag | Effect |
|---|---|
| `yoink exec <SERVICE> -- <CMD> <ARGS>...` | Runs one command in a running container. The exit code is the exit code of the command. |
| `yoink shell <SERVICE>` | Opens an interactive PTY shell (`bash`, or `sh` if there is no `bash`). With more than one replica, yoink selects the first healthy one and prints its name. Alias: `yoink ssh`. |
| `yoink debug <SERVICE>` | Opens a shell in a debug sidecar that shares the PID and network namespaces of the target. For distroless images and images without a shell. yoink removes the sidecar on exit. |
| `--image <IMAGE>` | Image of the sidecar. Default: `alpine`. |
| `yoink restart <SERVICE>` | Stops the container with a drain of 10 s and starts it again. Does not deploy. |
| `yoink kill <SERVICE>` | Sends SIGKILL to the container, with no drain. The container stays on the host. `--yes` skips the confirmation. |
| `--host <ADDRESS>` | On `exec`, `shell`, `debug`, `restart`, and `kill`: selects the replica on one host. |
| `yoink pf <SERVICE> [[LOCAL:]CONTAINER_PORT]` | Forwards a container port to your computer until Ctrl-C. See [Port-forward reference](/docs/reference/port-forward). |
| `--mode <auto\|published\|sidecar>` | Path selection. `auto` (default) uses a matching `publish:` entry, or a sidecar if there is none. `published` stops with an error when no entry matches. `sidecar` always starts a sidecar. |
| `-o, --open` | Opens the URL in the system browser when the tunnel is ready. |
| `--scheme <auto\|http\|https\|tcp\|none>` | Scheme of the printed URL. `auto` (default) selects by port. `tcp` and `none` print `localhost:N` and open no browser. |
| `--json` | Prints one JSON line (`service`, `host`, `mode`, `remote_ip`, `remote_port`, `container_port`, `local_port`, `url`), then holds the tunnel. |
| `-r, --replica <N>` | Replica index. Default: 0. |
| `--host <ADDRESS>` | Selects one host. Required when the service runs on more than one host. |
| `yoink vscode <SERVICE>` | Opens VS Code in your browser on the filesystem of a running container. See [yoink vscode sidecar](/docs/reference/vscode-sidecar). |
| `--no-open` | Prints the URL and opens no browser. |
| `--port <N>` | Local port. Default: OS-assigned. |
| `-r, --replica <N>`, `--host <ADDRESS>` | Same as for `pf`. |

### Audit log

| Command or flag | Effect |
|---|---|
| `yoink audit log` | Merges the operator-side log (`$XDG_STATE_HOME/yoink/audit/events.jsonl`) with `/var/lib/yoink/audit/events.jsonl` of each host. Removes duplicates by `event_id` and sorts newest first. |
| `--host <ADDRESS>` | Reads the log of one host only. |
| `--service <NAME>` | Shows the events of one service. |
| `--deploy-id <ID>` | Shows the events of one run. Matches a prefix. |
| `--event <NAME>` | Shows events with this name. Repeatable. |
| `--origin <operator\|host>` | Shows one side of the merge. |
| `--since <DURATION>` | Time window before now. Default: `7d`. |
| `--limit <N>` | Maximum number of rows. Default: 100. |
| `--format <text\|json>` | `json` prints raw JSONL. Default: `text`. |
| `yoink audit run <DEPLOY_ID>` | Shows every event of one run, on all hosts. Accepts `--format`. |
| `yoink audit gc` | Removes rotated audit files (`events-*.jsonl`) on each host that are older than `--keep`. Does not touch the active `events.jsonl`. |
| `--keep <DURATION>` | Retention window. Default: `90d`. |
| `--host <ADDRESS>` | Cleans one host only. |
| `--dry-run` | Prints the number of matching files and removes nothing. |

### Proxy and lock

| Command or flag | Effect |
|---|---|
| `yoink proxy-render` | Prints the Caddy admin-API JSON that yoink generates for the config. Read-only. Needs no host connection, because it uses service names in place of container lookups. |
| `yoink proxy-dockerfile` | Prints the generated xcaddy Dockerfile for `proxy.xcaddy:`. Makes no docker calls. |
| `yoink lock status` | Prints if the deploy lock is held on each host, and its age. |
| `yoink lock release --yes` | Force-removes the deploy lock on each host. Without `--yes`, the command stops. Safe only when no deploy is in progress. |
| `--host <ADDRESS>` | Releases the lock on one host only. |

### Secrets

| Command or flag | Effect |
|---|---|
| `yoink secrets key generate` | Generates an age keypair. Saves the identity (private) to `~/.config/yoink/keys/<recipient>.key` (mode 0600), where yoink finds it automatically. Prints the recipient (public, `age1…`) for `yoink.yaml`. |
| `--out <PATH>` | Writes the identity to `PATH` (mode 0600). Conflicts with `--print`. |
| `--print` | Prints the identity to stdout and writes no file. The recipient and notes go to stderr, for example `… --print \| gh secret set YOINK_AGE_KEY`. |
| `--force` | Overwrites an existing identity at the destination. |
| `yoink secrets key public` | Prints the recipient of the identity that yoink uses now. |
| `yoink secrets edit` | Decrypts the sealed file into `$EDITOR` and seals it again on save. Creates the file if it does not exist. The path is `secrets.file:` (default `secrets.age`). |
| `yoink secrets show` | Prints `KEY=value` with masked values. |
| `--reveal` | Prints the real values. Refused in CI unless `YOINK_ALLOW_REVEAL_IN_CI=1`. |
| `yoink secrets env` | Prints `export KEY='value'` lines for a local shell, for example `source <(yoink secrets env)`. Values are POSIX single-quoted. Same CI guard as `show --reveal`. |
| `--no-export` | Prints bare `KEY='value'` lines. |
| `--profile <NAME>` | Applies a `secrets.profiles` entry: keeps its `include` keys, renames by its `rename` map, and prints `unset 'KEY'` for each `unset` entry. When `$GITHUB_ENV` is set, also prints `::add-mask::` lines for each value. |
| `yoink secrets seal` | Merges plaintext keys into the sealed file. Reads a dotenv from stdin by default. |
| `--in <PATH>` | Reads the dotenv from `PATH`. Conflicts with `--as`. |
| `--as <KEY=VALUE>` | Sets one key. `--as KEY=@PATH` reads the value from a file. Repeatable. |
| `--out <PATH>` | Output path. Default: `secrets.file:`. |
| `--replace` | Replaces the full file with the input keys. Asks for confirmation when keys go away. |
| `-y, --yes` | Skips that confirmation. |
| `yoink secrets rotate` | Generates a new keypair and seals the file for the existing recipients and the new one. Prints the new identity. |
| `yoink secrets ssh-key generate --seal-as <NAME>` | Generates an ed25519 SSH keypair, seals the private key under `NAME`, and prints the public key. `--comment <TEXT>` sets the key comment. |
| `yoink secrets ssh-key public --name <NAME>` | Prints the public key of a sealed SSH private key. |

## Resolving tags

`yoink up` resolves the tag of each selected service in this order:

1. `--tag name=value`.
2. A bare `--tag value`, which applies to every service selected with `--service`.
3. The `tag:` field in the service config.
4. With `--service` and no `--tag`: the git short SHA, for each selected service without `tag:`.

Limits:

- A bare `--tag` without `--service` is an error. More than one bare `--tag` is an error.
- Rule 4 needs a clean working tree. It does not apply with `--allow-dirty`.
- With `--build`, a `build:` service without a tag gets a tag from the content digest of the built image.
- If no rule gives a tag, yoink stops with an error.

`--here` applies the git short SHA of `HEAD` to every selected service. It also overrides a `tag:` in the config, and it does not look at uncommitted changes.

## Tag overrides

```sh
yoink up --tag api=$(git rev-parse HEAD) --tag web=$(git rev-parse HEAD)
yoink up --service api --tag $(git rev-parse HEAD)         # bare form, single service
yoink up --here                                             # shorthand for "every service at HEAD"
yoink up --service api --here                               # one service at HEAD
```

## Force redeploy (`--force`)

`--force` skips the early return for services that are at spec. Every selected service runs the full prepare and finalize loop, which includes the Caddy admin push for routed services. It is the recovery flag for proxy state that drifted from the real containers, for example a stale upstream pool.

```sh
yoink up --service yoink-proxy --force          # re-push Caddy admin config
yoink up --service api --force                  # re-create + healthcheck all api replicas
```

An at-spec container keeps its name, so the prepare phase removes the old container before the new one starts. The service is down for a short time (approximately the healthcheck time).

## Watch mode (`--watch`)

`yoink up --watch` reconciles one time. Then it loads the config again every 2 s and reconciles on each change:

```sh
yoink up --watch --build --service my-tool
# edit Dockerfile / yoink.yaml → save → yoink rebuilds and ships
# Ctrl-C to exit
```

yoink prints config errors and reconcile errors to stderr, and the loop continues.

## Boolean-flag conventions

| Shape | Meaning | Examples |
|---|---|---|
| `--<verb>` | Enables an additive behavior. Off by default. | `--build`, `--watch`, `--push`, `--json`, `--force` |
| `--no-<thing>` | Disables a behavior that is on by default. | `--no-registry`, `--no-port`, `--no-secrets`, `--no-cache`, `--no-open` |
| `--allow-<safety>` | Overrides a safety guard that refuses by default. | `--allow-dirty` |

yoink does not use `--skip-X` or `--without-X`. `--yes` skips confirmation prompts and enables no other behavior.

## Dynamic shell completion

`yoink completions <shell>` prints static completions: subcommand names, flag names, and value enums. For dynamic values, the hidden `__complete` helper prints one value on each line:

| Subcommand | Source | Needs a config? |
|---|---|---|
| `yoink __complete services` | Service names from the current config. | yes |
| `yoink __complete hosts` | Host addresses from the current config. | yes |
| `yoink __complete configs` | `yoink.yaml` and `*.yoink.yaml` files below the current directory, newest first. | no |

`__complete configs` stops at depth 8. It skips `.git`, `.terraform`, `.cache`, `.next`, `.nuxt`, `.svelte-kit`, `.venv`, `venv`, `__pycache__`, `node_modules`, `target`, `dist`, and `build`.

## See also

- [Configuration reference](/docs/reference/config): the schema for `yoink.yaml`.
- [TUI reference](/docs/reference/tui): the keybinds for the terminal dashboard.
- [Add dynamic shell completion](/docs/how-to/dynamic-shell-completion): the Bash and Zsh snippets for the `__complete` helper.
- [Edit-save-deploy with `--watch`](/docs/how-to/watch-mode): the procedure for the watch loop.
