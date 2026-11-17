---
title: Keep the age key in the macOS Keychain
description: Move the age identity from a plaintext key file into the macOS login keychain, and keep a backup in 1Password.
---

By default, the age identity is a plaintext file in `~/.config/yoink/keys/`. On macOS, yoink can keep it in the login keychain. Then no plaintext copy is on disk or in your backups.

The keychain is opt-in. [Secrets](/docs/explanation/secrets#the-key-in-the-macos-keychain) explains what it protects and what it does not.

## Prerequisites

- macOS. On other systems, `--keychain` stops with an error.
- yoink on your Mac, and a project with `provider: age`.

## Move an existing key into the keychain

### Import the key file [step]

Run this command in the project directory:

```sh
yoink secrets key import --keychain
```

yoink finds the key file for the recipients of the project. It stores the identity in the keychain and reads it back. Then it asks before it deletes the file. To delete without a question, add `-y`. If the file holds more than one identity, yoink imports the first one and keeps the file.

To import a different file, give its path:

```sh
yoink secrets key import --keychain ~/keys/prod.key
```

### Make sure that yoink uses the keychain [step]

```sh
yoink secrets key list
```

```text
* keychain  age1qz…
```

The first row is the identity that yoink uses. If a `file` row shows the same recipient, the key is still on disk. Delete that file.

### Keep a backup [step]

A key that is only in the keychain is easy to lose. A reset of the login keychain removes it. A new Mac that you configure without Time Machine or Migration Assistant does not have it. Without the key, you cannot decrypt `secrets.age`.

Before you continue, do one of the two procedures in [Keep a backup in 1Password](#keep-a-backup-in-1password).

## Create a new key in the keychain

### Generate the identity [step]

```sh
yoink secrets key generate --keychain
```

The identity goes directly to the keychain. yoink prints the recipient (`age1…`).

### Add the recipient to the config [step]

```yaml
secrets:
  provider: age
  recipients:
    - age1qz…
```

yoink finds the key in the keychain when its recipient is in `secrets.recipients:`.

## Keep a backup in 1Password

The examples use the [1Password CLI](https://developer.1password.com/docs/cli/) (`op`). Any password manager with a CLI works the same way.

### Option 1: a second recipient

Use this option for a project that more than one person or system decrypts. The backup is a different key. The keychain key does not leave the keychain, and you can revoke the backup key independently.

1. Generate a backup identity and send it directly to 1Password:

   ```sh
   yoink secrets key generate --print | op document create - --title "yoink backup key (my-project)" --file-name backup.key
   ```

   yoink prints the recipient of the backup identity on stderr.

2. Add that recipient to `secrets.recipients:` in `yoink.yaml`.
3. Seal the file again for the new list of recipients:

   ```sh
   yoink secrets seal < /dev/null
   ```

4. Commit `yoink.yaml` and `secrets.age`.

To use the backup, get the key from 1Password into the environment for one command:

```sh
YOINK_AGE_KEY="$(op document get "yoink backup key (my-project)")" yoink secrets show
```

### Option 2: a copy of the key

Use this option for a project that only you decrypt.

Send the identity from the keychain to 1Password. The key does not go to disk or to the screen:

```sh
yoink secrets key export --keychain | op document create - --title "yoink age key (my-project)" --file-name age.key
```

To restore the key on a new Mac:

```sh
op document get "yoink age key (my-project)" | yoink secrets key import --keychain -
```

With `-`, yoink reads the identity from stdin and writes no file.

### Without the `op` CLI

1. Run `yoink secrets key export --keychain`.
2. Copy the `AGE-SECRET-KEY-1…` line into a secure note in your password manager.
3. Clear the scrollback of your terminal.

## Remove a key from the keychain

```sh
yoink secrets key remove --keychain age1qz…
```

yoink asks for a confirmation. Make sure that you have a backup before you remove the key.

## Troubleshooting

**macOS asks for the keychain password after an upgrade.** The release binaries are not code-signed, and macOS connects a keychain item to the binary that created it. A new binary is a different program to macOS. Enter the password and select "Always Allow".

**macOS asks on each run.** You selected "Allow" on the prompt, which permits one read. Select "Always Allow". yoink reads the key one time for each run, and only for a command that needs it.

**A script or a cron job on the Mac stops and does not continue.** macOS shows the keychain prompt and waits for an answer. yoink cannot continue until you answer. For an unattended run, set `YOINK_AGE_KEY`. yoink then does not read the keychain.

**`access to the keychain item … was denied`.** You selected "Deny" on the system prompt. yoink does not continue with a key file after a refusal. Run the command again and allow access, or set `YOINK_AGE_KEY`.

**`the keychain could not be read` in an SSH session into the Mac.** The login keychain is locked there. yoink continues with the key files and the environment variables, and it shows this error when they hold no identity. Set `YOINK_AGE_KEY` for that session.

**`yoink doctor` shows "the age identity is in the keychain and in a key file".** Delete the key file. yoink reads the keychain copy.

## See also

- [Secrets](/docs/explanation/secrets): the threat model, and what the keychain protects.
- [Secrets resolution](/docs/reference/secrets#identity-resolution): the lookup order and the keychain item.
- [Sealed secrets workflow](/docs/how-to/sealed-secrets-workflow): rotation and recipients.
- [CLI reference](/docs/reference/cli): the `yoink secrets key` commands.
