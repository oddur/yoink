---
title: Why `--watch` polls
description: Why yoink up --watch polls the config on a 2 s tick and does not use a file watcher, and what that costs.
---

`yoink up --watch` reads the config file and its include fragments again on a 2 s tick. It does not subscribe to file events.

## Why polling?

- The TUI of yoink uses the same 2 s tick to reload the config.
- Polling avoids a dependency on a file watcher of the `notify`/`fsnotify` type.
- Polling is fast enough for a person who types in an editor. The latency of the reconcile dominates any interval below ~500 ms.

## Tradeoffs vs. an inotify-based watcher

Polling adds a maximum of 2 s of latency between a save and the reconcile. In an editor flow, that latency is hard to see. If you chain `yoink up --watch` into a tighter feedback loop (a test runner, a screen recorder), 2 s is the floor.

A tick costs nothing on the hosts when the config did not change: yoink compares the new config with the last one and reconciles only on a difference. A reconcile that changes nothing is also cheap. yoink computes a [`yoink.spec_hash`](/docs/explanation/architecture#drift-detection) from the desired spec. If the label on the running container already matches, yoink stops there.

## See also

- [Edit-save-deploy with `--watch`](/docs/how-to/watch-mode): the commands for the watch loop.
- [Architecture: drift detection](/docs/explanation/architecture#drift-detection): how `yoink.spec_hash` finds a change.
- [CLI reference](/docs/reference/cli): `--watch` and the other `yoink up` flags.
