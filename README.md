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

## 📚 Full docs: <https://yoink.is/>

The README is a pointer; the [docs site](https://yoink.is/) is canonical.

- [Is this for me?](https://yoink.is/docs/intro/compared) — vs. Kamal, Kubernetes, plain compose
- [Five-minute first deploy](https://yoink.is/docs/start/first-deploy) — install + run
- [Three deploy modes](https://yoink.is/docs/guide/deploy-modes), [Secure by default](https://yoink.is/docs/guide/security), [Pairing with Tailscale + caddy](https://yoink.is/docs/guide/networking)
- [Recipes](https://yoink.is/docs/recipes) — staging alongside prod, self-hosted registry, external secrets via CLI, PR-comment dry-run
- [Reference](https://yoink.is/docs/reference) — CLI / config schema / TUI keybinds

## Install

```sh
brew install oddur/yoink/yoink
```

Or `cargo install --git https://github.com/oddur/yoink yoink`. Or curl-pipe-sh: `curl -LsSf https://github.com/oddur/yoink/releases/latest/download/yoink-installer.sh | sh`.

See the [install page](https://yoink.is/docs/start/install) for the full list and prerequisites.

## License

MIT — see [LICENSE](LICENSE).
