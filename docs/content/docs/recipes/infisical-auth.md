---
title: Authenticate to Infisical
weight: 3
---

When `secrets.provider: infisical` is set, yoink resolves a bearer token in this order. First match wins; failures cascade to the next.

{{< callout type="info" >}}
Yoink talks to Infisical's REST API directly — the `infisical` CLI binary is **not** invoked at deploy time, only its cached session (when present) is read.
{{< /callout >}}

{{% steps %}}

### Universal Auth (CI path)

Set both:

```sh
export INFISICAL_CLIENT_ID=<machine-identity-client-id>
export INFISICAL_CLIENT_SECRET=<machine-identity-client-secret>
```

In CI, expose them as repo secrets and inject into the deploy job's env. Yoink calls `/api/v1/auth/universal-auth/login` to exchange the pair for a short-lived bearer.

Create the machine identity in Infisical's UI under **Identities → Machine Identities**. Scope it to the project + environment(s) yoink needs.

### Raw bearer token (one-off)

```sh
export INFISICAL_TOKEN=<bearer>
```

Useful when you've already minted a token elsewhere (CI workflow, ephemeral runner, etc.).

### Cached browser-flow login (laptop dev)

Run `infisical login` once on your machine — yoink reads the session that the CLI persisted:

```sh
brew install infisical/get-cli/infisical
infisical login                         # add --domain=… for self-hosted
```

That stores a session token in your OS keyring (macOS Keychain, libsecret on Linux, Credential Manager on Windows). Yoink looks it up via the same key the CLI uses (`infisical-cli` service, account = your email from `~/.infisical/infisical-config.json`).

The CLI binary itself is never invoked at deploy time. `infisical login` just bootstraps the session; yoink talks to Infisical's REST API directly.

{{% /steps %}}

## Self-hosted Infisical

If you're running Infisical on your own infra, set the domain in the `secrets:` block:

```yaml
secrets:
  provider: infisical
  project_id: <uuid>
  environment: prod
  domain: https://infisical.my-tailnet.ts.net
```

Pairs cleanly with running Infisical itself as a yoink-managed service on the tailnet — same pattern as [self-hosted registry](/docs/recipes/self-hosted-registry).
