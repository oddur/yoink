---
title: Secrets resolution and bundle formats
description: "The order in which yoink finds an age identity, the rules for provider command, and the dotenv and JSON bundle formats."
---

For the `secrets:` fields of `yoink.yaml`, see the [Configuration reference](/docs/reference/config). For the design, see [Secrets](/docs/explanation/secrets).

## Identity resolution

To decrypt, yoink tries these sources in order and stops at the first match:

| Order | Source | When yoink uses it |
|---|---|---|
| 1 | `YOINK_AGE_KEY` env (raw key) | CI and managed-env contexts |
| 2 | `YOINK_AGE_KEY_FILE` env (path) | Explicit override |
| 3 | macOS login keychain | macOS only, opt-in. yoink asks the keychain for one item for each recipient in `secrets.recipients:`. |
| 4 | `~/.config/yoink/keys/*.key` | Laptop default. yoink scans the directory and picks the key whose public half matches one of the `secrets.recipients:` in `yoink.yaml`. |
| 5 | `~/.config/yoink/age.key` | Legacy single-key path, for backward compatibility. |

`yoink secrets key generate` writes to `~/.config/yoink/keys/` by default. `yoink secrets key public` prints the recipient of the identity that resolved. `yoink secrets key list` shows each identity that yoink finds for the project, and its source.

### Keychain items

`yoink secrets key generate --keychain` and `yoink secrets key import --keychain` store an identity in the macOS login keychain.

| Property | Value |
|---|---|
| Kind | Generic password |
| Service | `is.yoink.age-identity` |
| Account | The age recipient (`age1…`) |
| Name | `yoink age key: <project>`. The project is the name of the directory that holds the config. yoink writes the name when it creates the item. |
| Value | The identity (`AGE-SECRET-KEY-1…`) |
| Sync | None. The login keychain does not sync to iCloud. |

yoink finds an item by its service and account. The name is for you: it shows the project in Keychain Access and in `yoink secrets key list --all`. yoink does not search the keychain to decrypt. A command that has no config has no recipients, so it does not read the keychain.

yoink reads the keychain only for a command that needs the identity, and one time at most for each recipient in a run. A command needs the identity when it decrypts `secrets.age`. Examples are `yoink up`, `yoink secrets show`, and `yoink doctor`. `yoink validate` and `yoink proxy-render` decrypt the file when the config has routed services. A config with `address_secret:` or `ssh_key_secret:` on a host needs the identity for each command that connects to that host, for example `yoink status`. `yoink completions` does not read the keychain. TAB completion never opens the prompt: if macOS needs your permission to read the key, completion of host names with `address_secret:` shows no names. `yoink secrets key list` never opens the prompt. It reads the names of the items, and it reads the key only if macOS permits that without a question.

| Case | Result |
|---|---|
| No item for any recipient | yoink continues with the key files. |
| An SSH session into the Mac | yoink never opens the prompt, because macOS cannot show it there. If macOS permits the read without a question, yoink uses the key. If not, yoink continues with the key files. If no key file holds the identity, the error says that the keychain does not work in an SSH session. |
| A locked keychain | yoink continues with the key files. If no key file holds the identity, the error names the keychain. |
| You select "Deny" on the system prompt | yoink stops with an error. It does not read the key files. |
| The same identity is in the keychain and in a key file | yoink uses the keychain. `yoink doctor` shows a warning. |

The release binaries are not code-signed. macOS connects a keychain item to the binary that created it. As a result, macOS shows the keychain prompt again after each upgrade of yoink, and it shows the prompt to each other program that asks for the item. While the prompt is open, yoink waits.

## `provider: command`

```yaml
secrets:
  provider: command
  command: [...]
  format: dotenv     # or `json`; `auto` is the default
```

| Behavior | Rule |
|---|---|
| When the command runs | One time for each `yoink up`, and one time on TUI startup. |
| How the command runs | yoink spawns the binary directly with the supplied argv, with no `sh -c` and no shell interpolation. Pipes and env-var expansion need a shell script as the command. |
| stdout | yoink parses all of stdout as the secrets bundle. Progress bars, `Logging in…` banners, and `set -x` traces break the parse. Diagnostics must go to stderr (`>&2`). |
| stderr | yoink captures stderr and shows it when the command exits non-zero. |
| Limits | 10 MB of stdout, and 60 s wall-clock for the call. |
| `format: auto` | Stdout whose first non-whitespace byte is `{` is JSON. All other stdout is dotenv (`KEY=value\n`). |
| `format: dotenv`, `format: json` | Forces the parser, for a bundle that the detection reads incorrectly. |

### Spawn environment

yoink forwards only these variables to the spawned command:

| Group | Variables |
|---|---|
| Base | `PATH`, `HOME`, `USER`, `XDG_CONFIG_HOME`, `LANG`, `LC_ALL`, `TERM`, `TZ` |
| Doppler | `DOPPLER_TOKEN` |
| Infisical | `INFISICAL_TOKEN`, `INFISICAL_CLIENT_ID`, `INFISICAL_CLIENT_SECRET` |
| 1Password | `OP_SERVICE_ACCOUNT_TOKEN`, `OP_CONNECT_HOST`, `OP_CONNECT_TOKEN` |
| Vault | `VAULT_ADDR`, `VAULT_TOKEN`, `VAULT_NAMESPACE`, `VAULT_CACERT` |
| AWS | `AWS_PROFILE`, `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN`, `AWS_SHARED_CREDENTIALS_FILE`, `AWS_CONFIG_FILE` |
| Google Cloud | `GOOGLE_APPLICATION_CREDENTIALS` |
| Azure | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` |
| Bitwarden | `BWS_ACCESS_TOKEN` |
| sops | `SOPS_AGE_KEY`, `SOPS_AGE_KEY_FILE` |

- yoink does **not** forward `YOINK_AGE_KEY`.
- Other env vars reach the command only through a wrapper script that exports them again.
- Every allowlisted variable in your shell reaches the command, whichever provider it runs. For example, `SOPS_AGE_KEY` and `DOPPLER_TOKEN` both reach a Doppler command. A wrapper that scrubs the environment around yoink prevents this.

## `secrets.file:` paths

`secrets.file:` rejects `..` and absolute paths. This rule does not resolve symlinks. For a committed sealed file that is a symlink:

- On read, the age decrypt runs on the bytes of the target and fails if the target is not a sealed file.
- On write, `rename(2)` replaces the symlink, and the original target stays untouched.

yoink does not guard against a symlinked sealed file in the repo. PR review must catch it.

## Format reference

A malformed bundle is a hard error. yoink does not skip a line that it cannot parse.

### dotenv

The default for stdout that does not start with `{`:

```dotenv
# comments and blank lines OK
KEY=value
QUOTED="value with spaces"
SINGLE_QUOTED='value'
export PREFIX_OK=true     # leading `export ` stripped

# Quoted values may span multiple lines — useful for PEM certs and
# private keys, which is what `infisical export --format=dotenv`,
# `doppler secrets download --format env`, and shell `set` emit
# without you having to do anything.
TLS_CERT='-----BEGIN CERTIFICATE-----
MIIErDCCA5SgAwIBAgIUd...
-----END CERTIFICATE-----'
```

- yoink strips a UTF-8 BOM at the start (`\xEF\xBB\xBF`). Windows tooling sometimes emits one.
- For a key that occurs twice, the later line wins.

### JSON

For stdout that starts with `{`:

```json
{"KEY":"value","QUOTED":"value with spaces","NESTED_NULL_DROPPED":null}
```

- The top level must be an object of `string → string` pairs.
- yoink ignores `null` values.
- **Non-string scalars (numbers, booleans) and nested objects or arrays are a hard error.** yoink does not convert `8080` to `"8080"` or `true` to `"true"`.
- A numeric or boolean secret must be a JSON string at the source.

## See also

- [Secrets](/docs/explanation/secrets): the model, the threat model, and when to use each provider.
- [Read secrets from an external secret manager](/docs/how-to/external-secrets): `provider: command` config for sops, Doppler, Vault, 1Password, and more.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow): the tasks for `provider: age`.
- [Configuration reference](/docs/reference/config): the `secrets:` schema for the two providers.
