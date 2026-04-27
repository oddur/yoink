# Sealed-secrets test fixture

Self-contained walkthrough that exercises age-sealed secrets end-to-end against your laptop's docker daemon. No ssh, no registry, no remote services.

## Setup (~30 seconds)

```sh
cd examples/sealed-secrets

# 1. Generate an age identity. Writes the secret to ~/.config/yoink/age.key
#    (0o600); prints the public recipient.
yoink secrets keygen

# 2. Paste the public key into yoink.yaml under `secrets.recipients:`,
#    replacing the `age1REPLACE_ME_...` placeholder.

# 3. Seal a value into secrets.age.
printf 'GREETING=hello sealed world\n' | yoink secrets seal

# 4. Reconcile — pulls alpine, injects GREETING from secrets.age,
#    starts the demo container.
yoink up

# 5. Verify the value lands inside the container.
yoink exec demo -- env | grep GREETING
# → GREETING=hello sealed world
```

## What this exercises

- `secrets.provider: age` schema parsing
- `yoink secrets keygen / seal` round-trip on disk
- The deploy-time decryption path (`secrets::load_bundle`)
- Per-service `secrets:` injection as env vars
- spec_hash inclusion of secret values — change the value, re-seal, run `yoink up` again, and the container is replaced (rather than reused) because the hash differs.

## Iterating

Edit the sealed file in `$EDITOR` to add / change keys:

```sh
yoink secrets edit
```

Or interactively from the TUI:

```sh
yoink tui
# press `e` to enter the secrets pane
# `a` to add, `e` to edit, `d` to delete, `r` to reveal/mask
```

After any edit, `yoink up` re-deploys the demo container with the new env (drift detection picks it up automatically).

## Tear down

```sh
yoink kill demo --yes      # stop the container
docker rm demo             # remove it (yoink prune would too, once the config is gone)
docker network rm demo     # remove the network
rm secrets.age             # discard the sealed bundle
```

Or just `docker system prune` if you don't mind the broader cleanup.

## What's NOT in scope

- **Multiple hosts**: `local` only. For multi-host distribution see [docs/recipes/multi-host-distribution](https://oddur.github.io/yoink/docs/recipes/multi-host-distribution).
- **Registry auth**: this fixture pulls `alpine` from Docker Hub anonymously. For private registries, see the `registry:` block in the [config reference](https://oddur.github.io/yoink/docs/reference/config).
- **CI integration**: the [AGE secrets in GitHub Actions recipe](https://oddur.github.io/yoink/docs/recipes/age-in-github-actions) covers paste-into-secret + workflow wiring.

## Note on `secrets.age` in this directory

This directory does NOT commit a real `secrets.age`. The `.gitignore` here ignores it — every operator generates their own identity + sealed file. If you want to commit a sealed file for a real environment (which is the whole point of age), check it in from your repo root, not from this fixture.
