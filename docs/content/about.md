---
title: About
layout: docs
toc: false
weight: 99
---

# About

Yoink came out of a side project I wanted to run on a couple of VPS servers.

I'd spent years deep in the CNCF / Kubernetes world, and there are real things to love there — drift detection, healthchecks gating rollouts, dependency-ordered waves, a TUI to actually see what's happening, sealed secrets in the repo, all of it. But I didn't want a control plane, a Helm chart, an operator, an admission webhook, and a stack of YAML to deploy a Postgres next to a Rust binary on a €4/month box.

So yoink is a swiss army pocket knife of the utilities I missed, mixed with my own opinions about how to operate a handful of services on a handful of hosts without drowning in complexity. It's the deploy tool I wanted to have, and didn't.

There are good tools in this neighborhood already — Kamal, Dokku, Coolify, plain `docker compose`, hand-rolled bash. None of them quite did what I wanted in the way I wanted it. See the [comparison page](/docs/intro/compared) for the honest version of where each one sits and where yoink ends up.

It was originally meant for small side projects, sharing a prototype, running an experiment cheaply on a VPS without spinning up a registry and a CI pipeline first. It still is. But the same primitives — healthcheck-gated swaps, pre-deploy migrations, sealed secrets, multi-host fan-out — also operate a production-quality setup if you wield it that way.

In fact, we dogfood it to serve this very site off a $4/mo VPS — [check out how](https://github.com/oddur/yoink/blob/main/docs/yoink.yaml).

If yoink ends up useful to you, I'd love to hear about it — [oddur.me](https://oddur.me).

PRs, issues, and template contributions are very welcome. The repo is at [github.com/oddur/yoink](https://github.com/oddur/yoink). If you've built a yoink template for a service worth sharing (databases, caches, search, object storage, backup tooling), open a PR against [`templates/`](https://github.com/oddur/yoink/tree/main/templates).

— Oddur
