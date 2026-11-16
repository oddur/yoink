---
title: How yoink builds Caddy plugins
description: Why yoink compiles a custom Caddy on each proxy host, when it rebuilds, and what the build costs.
---

Caddy plugins are Go modules that the `xcaddy` tool links statically into a custom binary. yoink runs that build on each proxy host and stores the result in the local image cache of the host.

## Why the build runs on each host

yoink does not assume that you have a registry. A build on each host needs no registry, which fits a fleet behind a tailnet. It also removes the work to maintain a registry for one image.

The feature does not build one time and ship the image to many hosts. If you have a registry, a hand-built xcaddy image with `proxy.image:` gives that model.

## What happens during `yoink up`

The build runs in the image prefetch phase, at the same time as the image pulls and before any service deploys. yoink does these steps in parallel on every host that runs the proxy:

1. yoink computes a content hash of `(caddy_version, base_image, builder_image, sorted plugin list, env, sorted replace list)`. The hash gives the local tag `yoink-caddy:<hash>`.
2. yoink asks the docker daemon of the host if that tag is in the cache. If it is, the full xcaddy step is a no-op.
3. If the tag is not in the cache, yoink synthesizes a two-stage Dockerfile (`caddy:2-builder` → `caddy:2`). It packs the Dockerfile into an in-memory tar context and streams it to the docker daemon as a `POST /build`. The stream uses the same SSH transport that yoink uses for all other operations.
4. The build tags the image `yoink-caddy:<hash>` on that host. The labels `yoink.caddy.xcaddy_hash=<hash>` and `yoink.caddy.plugins=<csv>` identify the image in `docker images`.

The proxy service then deploys from the local tag. Subsequent `yoink up` runs stop at step 2 and complete in seconds.

The Dockerfile is small, and you can read all of it. The [xcaddy build reference](/docs/reference/xcaddy#generated-dockerfile) shows an example.

## Why a new caddy release does not start a rebuild

The hash is the contract. The same inputs always give the same tag, and a new upstream caddy release is not an input. This trade-off is intentional: deterministic builds are better than rebuilds that you did not expect. The [rebuild rules](/docs/reference/xcaddy#rebuild-rules) list each change and its effect. [Force a rebuild](/docs/how-to/caddy-plugins#force-a-rebuild) gives the procedure to get the latest caddy.

## The cost of the first build

Each fresh host pays for the full xcaddy compile one time. The compile takes approximately 2-5 minutes for typical plugin sets. That time covers the download of the Go toolchain, the module fetches, and the link step.

The builds run in parallel across hosts, thus the total time is the `max` of the hosts and not the `sum`.

yoink forwards the build stream of bollard to `tracing::debug!`. The default INFO logging shows only deploy milestones. Without DEBUG, the terminal shows nothing during the compile.

## Old images stay on the host

A change of the plugin set gives a new tag, and the image with the old tag stays in the image cache. yoink does not prune these images automatically. You decide when to reclaim the disk space. [Remove stale images](/docs/how-to/caddy-plugins#remove-stale-images) gives the commands.

## Why plugin directives need JSON snippets

yoink adapts `caddy_extra_caddyfile:` snippets to JSON on your computer. It uses the adapter of the bundled `caddy:2` image, which does not know the directives that plugins supply. A plugin directive in a Caddyfile snippet thus fails with a `caddy adapt` error.

`caddy_extra_json:` does not go through the adapter. Caddy validates that JSON at `/load` time on the host, where the binary contains the plugin. Thus `caddy_extra_json:` is the supported path for snippets that use plugins.

## What `yoink validate` skips

`yoink validate` normally runs `docker run <proxy-image> caddy validate` on your computer to find schema errors before a deploy. With `proxy.xcaddy:` set, the proxy image exists only on the hosts. `yoink validate` then skips the docker step and prints a message. The step that generates the config still runs and reports its errors. As a last defense, Caddy refuses bad configs at `/load` time on the host.

## When a proxy cache helps

The `caddyserver/cache-handler` plugin caches responses at the proxy. It helps in these conditions:

- Responses with high fan-out, such as a homepage, marketing pages, and public API endpoints, where the origin recompute is expensive.
- Slow origins (Rails, Django, or PHP rendering) behind Caddy on the same host or one tailnet hop away.
- Burst protection in front of an endpoint that depends on a database. Short TTLs (5-30s) collapse a traffic spike into one origin hit.
- Asset proxies where the upstream is a slow blob store.

It does not help in these conditions:

- **A CDN edge such as Cloudflare, Fastly, or Bunny is in front.** The CDN caches before the origin proxy. cache-handler then does the same work with worse hit rates, because each yoink host has its own cold cache. One rare exception is cache warmup for purge events: the proxy keeps the response when the CDN fetches again.
- **Responses are highly personalized**, such as per-user dashboards and signed-in feeds. Either the origin sets `Cache-Control: private` and the cache handler skips the responses, or you configure cache keys with `Vary: Authorization`. The second method becomes fragile quickly.
- **Strong consistency is a hard requirement**, for example for financial state. A cache at the proxy adds a delay between writes and reads.

## See also

- [Caddy plugins (xcaddy, no registry)](/docs/how-to/caddy-plugins): the procedure to add plugins and the recipes for common ones.
- [xcaddy build reference](/docs/reference/xcaddy): image tags, labels, rebuild rules, and a plugin list.
- [Reverse proxy](/docs/explanation/proxy): how yoink models routes in Caddy.
- [Config reference](/docs/reference/config#xcaddy): the fields of `proxy.xcaddy:`.
