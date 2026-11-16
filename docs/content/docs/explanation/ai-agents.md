---
title: yoink and AI agents
description: Why a CLI, YAML files, and deterministic exit codes let Claude Code, Cursor, Aider, and GitHub Copilot Workspace operate yoink.
weight: 8
---

The full control surface of yoink is a CLI binary and YAML files in your repo. There is no web dashboard and no REST API to learn. Thus an AI coding agent that can work at a terminal can operate yoink. Examples are Claude Code, Cursor, Aider, OpenAI Codex, GitHub Copilot Workspace, and Devin.

## Why it works

| Property | Why an agent benefits |
|---|---|
| **Single binary, on-demand** | The agent runs `yoink up` as it runs any other shell command. There is no long-running service to start and no auth flow to script. |
| **YAML-only config** | The agent reads, edits, and greps `yoink.yaml` as it does with any other source file. Diffs are clean in PRs. |
| **`--dry-run --format=markdown`** | The agent sees what changes before it applies the change. The output goes back into the conversation as text. |
| **Deterministic exit codes** | Success and failure are unambiguous. The agent does not interpret a screen. |
| **Idempotent** | After a partial failure, a second `yoink up` continues from where the first one stopped. A retry is safe. |
| **Drift detection in labels** | `docker inspect` or `yoink status` shows if the live state matches the declared spec. The agent does not have to trust its own memory. |
| **Same workflow local & CI** | The commands that the agent runs locally work the same in a GitHub Actions runner. |

## The agent-friendly loop

The loop has five parts. Each part is one shell command with predictable output, so the agent does not scrape a screen or wait for a UI.

- The agent describes the change in `yoink.yaml`.
- It shows the diff with a dry run.
- It applies the change with `yoink up`.
- It makes sure that the rollout is good with `yoink status` and a request to the public endpoint.
- If the rollout is bad, it rolls back with `yoink rollback`.

A failed healthcheck needs no human to recover. `yoink up` exits with a non-zero code and prints the last 30 log lines of the failed container. yoink removes the containers of the failed attempt, and the old containers keep running.

A human uses the same loop. A prompt for a deploy change works best when it describes the **YAML diff** and not the deploy outcome.

For the steps and an example prompt, see [Drive yoink from an AI agent](/docs/how-to/drive-yoink-from-an-agent). For the commands that an agent uses most, see [Commands for agents](/docs/reference/agent-commands).

## CI runs the same loop

The GitHub Actions workflows in these docs use the same commands (see [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run)). A runner runs `yoink up --dry-run` and posts the diff to the PR. On merge, it runs `yoink up` without the dry run. Thus you can move the work of a local agent to CI with no new commands.

## What does not work yet

- **The TUI is keyboard-driven.** `yoink tui` is for humans. For inspection, an agent uses `yoink status`, `yoink history`, and `yoink logs`.
- **Some subcommands open an editor.** `yoink secrets edit` opens `$EDITOR`. An agent uses `yoink secrets seal --as KEY=value`, or it pipes a dotenv file into `yoink secrets seal`.
- **No streaming structured output.** `yoink up` writes human-readable progress lines, and the exit code gives success or failure. For structured output, an agent uses `yoink up --dry-run --format=json` or `yoink status --json`.

If an agent cannot work around a limitation, you can [open an issue](https://github.com/oddur/yoink/issues).

## See also

- [Drive yoink from an AI agent](/docs/how-to/drive-yoink-from-an-agent): the loop as a procedure, with an example prompt.
- [Commands for agents](/docs/reference/agent-commands): the subcommands that an agent uses most.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run): plan output that an agent can read, from `yoink up --dry-run --format=markdown`.
- [CLI reference](/docs/reference/cli): each subcommand that an agent can run.
