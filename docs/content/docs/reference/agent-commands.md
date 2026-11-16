---
title: Commands for agents
description: The yoink subcommands that an AI coding agent uses most.
---

```sh
yoink validate                          # Schema + Caddy-config sanity check, fast
yoink up --dry-run --format=markdown    # "What would happen?" — paste into PR comment
yoink up --service api                  # Reconcile one service in isolation
yoink status                            # One-shot table; parses cleanly
yoink history api                       # Who deployed what, when (last 20)
yoink rollback --service api            # Roll service back to its previous version
yoink logs api -f                       # Stream logs (terminate when done)
yoink prune --dry-run                   # See what stale containers would be removed
yoink proxy-render                      # Print the rendered Caddy config (debugging)
yoink pf api --json &                   # Tunnel to a (possibly non-published) service;
                                        # stdout is a JSON line {local_port, mode, url, …}
                                        # the agent can read to drive a follow-up curl
```

yoink has no hidden state. The [CLI reference](/docs/reference/cli) lists every subcommand and flag.

## See also

- [CLI reference](/docs/reference/cli): the full list of subcommands and flags.
- [Drive yoink from an AI agent](/docs/how-to/drive-yoink-from-an-agent): the procedure that uses these commands.
- [yoink and AI agents](/docs/explanation/ai-agents): why agents can operate yoink.
