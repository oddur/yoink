---
title: Driving yoink from an AI agent
weight: 8
---

Yoink is unusually well-suited to being driven by AI coding agents (Claude Code, Cursor, Aider, OpenAI Codex, GitHub Copilot Workspace, Devin, …) because **its entire control surface is a CLI binary plus YAML files in your repo** — no web dashboard, no REST API to learn, no interactive prompts to navigate. Anything an agent can do at a terminal, it can do with yoink.

This page is the short pitch + a few patterns that work well in practice.

## Why it works

| Property | Why an agent benefits |
|---|---|
| **Single binary, on-demand** | Agent runs `yoink up` like any other shell command. No long-running service to bootstrap, no auth flow to script. |
| **YAML-only config** | Agent can read, edit, and grep `yoink.yaml` like any other source file. Diffs show up cleanly in PRs. |
| **`--dry-run --format=markdown`** | Agent can ask "what would change?" before committing. Output is machine-readable enough to feed back into the conversation. |
| **Deterministic exit codes** | Success / failure is unambiguous. No "click the button and see what happens." |
| **Idempotent** | Re-running `yoink up` after a partial failure picks up where it left off. Safe to retry. |
| **Drift detection in labels** | Agent can `docker inspect` (or `yoink status`) to see whether the live state matches the declared spec, without trusting its own memory. |
| **Same workflow local & CI** | The agent's local commands work identically in a GitHub Actions runner. Nothing special to wire up. |

## The agent-friendly loop

The pattern that works:

1. **Describe the change in YAML.** Edit `yoink.yaml` to add a service, change an image tag, bump a replica count, etc.
2. **Preview the diff.** `yoink up --dry-run --format=markdown` — output is a markdown table the agent can paste into the chat or a PR comment.
3. **Apply.** `yoink up` (or scoped: `yoink up --service api`).
4. **Verify.** `yoink status` (one-shot table) and `curl` the public endpoint. The exit code on a failed healthcheck-gated swap is non-zero, so the agent knows immediately if rollout failed.
5. **Roll back if needed.** `yoink rollback api` — atomic, no human in the loop required.

Every step in this loop is a single shell command with predictable output. No screen-scraping, no waiting for a UI to render.

## Useful subcommands for agents

```sh
yoink validate                          # Schema + Caddy-config sanity check, fast
yoink up --dry-run --format=markdown    # "What would happen?" — paste into PR comment
yoink up --service api                  # Reconcile one service in isolation
yoink status                            # One-shot table; parses cleanly
yoink history api                       # Who deployed what, when (last 10)
yoink rollback api                      # Roll service back to its previous version
yoink logs api -f                       # Stream logs (terminate when done)
yoink prune --dry-run                   # See what stale containers would be removed
yoink proxy-render                      # Print the rendered Caddy config (debugging)
yoink pf api --json &                   # Tunnel to a (possibly non-published) service;
                                        # stdout is a JSON line {local_port, mode, url, …}
                                        # the agent can read to drive a follow-up curl
```

The full surface is in the [CLI reference](/docs/reference/cli) — all flags, all subcommands, no hidden state.

## A tip for prompt design

When asking an agent to make a deploy change, anchor it on the **YAML diff** rather than on the deploy outcome. A good prompt looks like:

> Add a new `worker` service to `yoink.yaml`: image `ghcr.io/me/worker:v1`, depends on `redis`, runs on the `api` and `redis` networks, healthcheck on `/health`. Then run `yoink up --dry-run` and show me the diff before applying.

The agent edits the file, runs the dry-run, you skim the diff, the agent applies. This is the same loop a human would follow — yoink doesn't ask the agent to do anything fundamentally different.

## CI is just an agent that doesn't talk back

The GitHub Actions workflows yoink documents (see [Pre-merge dry-run on every PR](/docs/recipes/pr-comment-dry-run)) are the same shape: an automated runner edits / reads `yoink.yaml`, calls `yoink up --dry-run`, posts the diff back to the PR, and on merge calls `yoink up` for real. Anything an AI agent does locally, you can graduate to CI by copying the same commands into a workflow file.

## What doesn't work yet

A couple of honest limitations:

- **The TUI is keyboard-driven.** `yoink tui` is for humans. Agents should stick to `yoink status`, `yoink history`, `yoink logs` for inspection.
- **Interactive prompts.** A few subcommands (`yoink secrets edit`) drop into `$EDITOR`. Agents should use `yoink secrets set KEY=value` style flags or pre-write the secrets file directly.
- **No streaming structured output.** `yoink up` emits human-readable progress lines. The exit code tells you success/failure; for richer machine-readable output, use `--dry-run --format=markdown` or parse `yoink status`.

These are tractable; if you hit a friction point that an agent can't work around, [open an issue](https://github.com/oddur/yoink/issues).

## See also

- [Pre-merge dry-run on every PR](/docs/recipes/pr-comment-dry-run) — agent-readable plan output via `yoink up --dry-run --format=markdown`.
- [CLI reference](/docs/reference/cli) — every subcommand surface an agent might drive.
