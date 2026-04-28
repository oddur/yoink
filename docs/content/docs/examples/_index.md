---
title: Examples
weight: 6
sidebar:
  open: true
---

Complete annotated `yoink.yaml`s — three real shapes. Lift them, edit until they fit your stack.

{{< cards cols="2" >}}
  {{< card link="/docs/examples/hobby-tool" title="Hobby tool / utility" subtitle="Single host, single service, build locally, no registry. The 5-line yoink.yaml." icon="cube" >}}
  {{< card link="/docs/examples/polyglot-stack" title="Polyglot stack" subtitle="Rust API + Node web + Caddy + Redis on a single host with per-tier networks." icon="server" >}}
  {{< card link="/docs/examples/production" title="Production-shape" subtitle="Multi-host, replicas, secrets, pre-deploy hooks, drift detection, the works." icon="briefcase" >}}
{{< /cards >}}

For a single fully-commented config naming every field, see [`examples/yoink.yaml` in the repo](https://github.com/oddur/yoink/blob/main/examples/yoink.yaml). The [sealed-secrets fixture](https://github.com/oddur/yoink/tree/main/examples/sealed-secrets) is a self-contained walkthrough that runs against your laptop's docker daemon — no remote host required.
