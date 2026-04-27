---
title: Recipes
weight: 4
sidebar:
  open: true
---

Task-oriented how-tos. Each recipe stands alone — pick the one that matches what you're doing.

{{< cards cols="2" >}}
  {{< card link="/docs/recipes/sealed-secrets" title="Sealed secrets (age)" subtitle="The default. Commit secrets.age, decrypt with one key. No remote vault needed." icon="lock-closed" >}}
  {{< card link="/docs/recipes/age-in-github-actions" title="AGE secrets in GitHub Actions" subtitle="Generate a CI-only identity, paste into a GitHub secret, deploy. One env var." icon="lightning-bolt" >}}
  {{< card link="/docs/recipes/staging-alongside-prod" title="Run staging alongside prod" subtitle="Same hosts, two configs, namespaced services + networks." icon="duplicate" >}}
  {{< card link="/docs/recipes/self-hosted-registry" title="Self-hosted registry on a yoink host" subtitle="Run registry:2 as a yoink service, expose via tailnet, push to it." icon="cube" >}}
  {{< card link="/docs/recipes/infisical-auth" title="Authenticate to Infisical" subtitle="Three modes: Universal Auth env vars (CI), raw bearer token, cached browser-flow login." icon="key" >}}
  {{< card link="/docs/recipes/pr-comment-dry-run" title="Pre-merge dry-run on every PR" subtitle="Sticky GitHub PR comment showing what `yoink up` would change before the merge." icon="annotation" >}}
  {{< card link="/docs/recipes/multi-host-distribution" title="Multi-host distribution" subtitle="`replicas` + `services[].hosts` patterns: scale-out, singletons, region-pinned, stateful + stateless." icon="server" >}}
{{< /cards >}}
