# yoink

A small, opinionated container deploy CLI + TUI for people who run a handful of services on a handful of bare-metal hosts. Sits between Kamal and Kubernetes — opinionated about the same things Kamal is, borrowing the few Kubernetes ideas that actually pay off at this scale.

```
yoink up                  # reconcile every service in dep order
yoink add postgres        # drop in vetted templates (postgres, redis, your own)
yoink tui                 # k9s-style dashboard, drift, logs, shell-into
yoink prune               # clean up stale containers
yoink history api         # who deployed what, when
yoink rollback api        # roll back to the previous version
```

## 📚 Full docs: <https://oddur.github.io/yoink/>

The README is a pointer; the [docs site](https://oddur.github.io/yoink/) is canonical.

- [Is this for me?](https://oddur.github.io/yoink/docs/intro/compared) — vs. Kamal, Kubernetes, plain compose
- [Five-minute first deploy](https://oddur.github.io/yoink/docs/start/first-deploy) — install + run
- [Three deploy modes](https://oddur.github.io/yoink/docs/guide/deploy-modes), [Secure by default](https://oddur.github.io/yoink/docs/guide/security-defaults), [Pairing with Tailscale + caddy](https://oddur.github.io/yoink/docs/guide/pairing)
- [Recipes](https://oddur.github.io/yoink/docs/recipes) — staging alongside prod, self-hosted registry, external secrets via CLI, PR-comment dry-run
- [Reference](https://oddur.github.io/yoink/docs/reference) — CLI / config schema / TUI keybinds

## Install

```sh
brew install oddur/yoink/yoink
```

Or `cargo install --git https://github.com/oddur/yoink yoink`. Or curl-pipe-sh: `curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh`.

See the [install page](https://oddur.github.io/yoink/docs/start/install) for the full list and prerequisites.

## License

MIT — see [LICENSE](LICENSE).
