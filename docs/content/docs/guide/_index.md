---
title: Guide
weight: 3
sidebar:
  open: true
---

The mental model. Read this once; come back to recipes / reference for the rest.

{{< cards cols="2" >}}
  {{< card link="/docs/guide/deploy-modes" title="Three deploy modes" subtitle="CI-built, kamal-style local-build, no-registry standalone." icon="server" >}}
  {{< card link="/docs/guide/security-defaults" title="Secure by default" subtitle="cap_drop=ALL, read-only rootfs, no-new-privileges, etc. — the hardened RunOptions every yoink container gets by default." icon="shield-check" >}}
  {{< card link="/docs/guide/pairing" title="Pairing with Tailscale + caddy" subtitle="What yoink owns and what it leaves to other tools." icon="link" >}}
{{< /cards >}}
