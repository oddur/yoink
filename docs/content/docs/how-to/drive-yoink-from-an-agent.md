---
title: Drive yoink from an AI agent
description: Give an AI coding agent a deploy change, preview the diff, apply it, and roll back if the rollout fails.
---

Make a deploy change through an AI coding agent. Each step is a single shell command with predictable output. For the reasons that this works, see [yoink and AI agents](/docs/explanation/ai-agents).

## Make a deploy change with an agent

### Write the prompt [step]

Describe the **YAML diff**, not the deploy outcome. Ask for a dry run before the agent applies the change. For example:

> Add a new `worker` service to `yoink.yaml`: image `ghcr.io/me/worker:v1`, depends on `redis`, runs on the `api` and `redis` networks, healthcheck on `/health`. Then run `yoink up --dry-run` and show me the diff before applying.

### Describe the change in YAML [step]

The agent edits `yoink.yaml`. For example, it adds a service, changes an image tag, or changes a replica count.

### Preview the diff [step]

```sh
yoink up --dry-run --format=markdown
```

The output is a markdown table that the agent can paste into the chat or into a PR comment. Read the diff.

### Apply the change [step]

```sh
yoink up
```

To apply the change to one service only, use `yoink up --service api`.

### Make sure that the rollout is good [step]

Run `yoink status`, which shows a one-shot table. Then `curl` the public endpoint.

If a healthcheck-gated swap fails, the exit code of `yoink up` is non-zero. Thus the agent knows that the rollout failed.

### If the rollout failed, roll back [step]

```sh
yoink rollback --service api
```

The rollback deploys the previous tag through the same rolling swap and healthcheck as `yoink up`. A human in the loop is not necessary.

## See also

- [yoink and AI agents](/docs/explanation/ai-agents): why a CLI and YAML files fit agents, and the limitations.
- [Commands for agents](/docs/reference/agent-commands): the subcommands that an agent uses most.
- [Pre-merge dry-run on every PR](/docs/how-to/pr-comment-dry-run): the same loop in GitHub Actions.
