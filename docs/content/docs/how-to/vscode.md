---
title: Edit container files with `yoink vscode`
description: Open VS Code in the browser on the live filesystem of a running container, with only SSH and a browser on your computer.
weight: 9
---

`yoink vscode <service>` opens VS Code in your browser, with the live filesystem of a running container as its root. A save writes directly to the files of the running container, with no copy, rebuild, or redeploy.

## Requirements

- The target service runs. `yoink vscode` finds it by the `yoink.service=<name>` label.
- SSH can reach the host. yoink rejects `address: local` because it has no tunnel endpoint.
- The service or `deploy.networks:` declares a network. The sidecar joins this network for DNS in the terminal.

## Open the editor

1. Run `yoink vscode` with the name of the service:

   ```sh
   yoink vscode api
   ```

   The first run pulls `codercom/code-server` (~700 MB) on the host. Subsequent runs use the cached image and start in ~3 s.

2. Edit the files in the browser.
3. Press Ctrl-C to stop. yoink closes the tunnel and force-removes the sidecar.

Optional flags:

| Flag | Use it when |
|---|---|
| `--host <addr>` | The service runs on more than one host. |
| `-r <n>` | The service has more than one replica (0-based, default `0`). |
| `--no-open` | You do not want yoink to open a browser. yoink prints the URL and waits. |
| `--port <n>` | You need a fixed local port on your laptop. |

## In-browser terminal caveat

The terminal in the code-server UI runs in the mount namespace of the **sidecar**, not of the target. In that terminal, `ls /` shows the filesystem of the code-server image. The file tree shows the filesystem of the target. For a shell in the target, run `yoink shell <service>`.

## Read-only mode

code-server has no read-only setting, so `yoink vscode` has no read-only mode. The risk of a bad write is the same as with `yoink shell`.

- Do not truncate database files.
- Do not run `rm -rf` on a path that the app holds an `fd` to.
- To examine a file without risk, first copy it out with `docker cp`.

## Every file read returns `EACCES`

A custom seccomp or AppArmor profile on the host can block `SYS_PTRACE`. Then the sidecar starts but every file read returns `EACCES`.

Correct this on the host: permit `SYS_PTRACE` in the seccomp or AppArmor profile.

## See also

- [`yoink vscode` sidecar](/docs/reference/vscode-sidecar): the sidecar properties, the filesystem view of the editor, and the flags.
- [`yoink pf`](/docs/reference/cli): the same SSH tunnel and sidecar design, for ports.
- [`yoink shell`](/docs/reference/cli): a shell in the PID namespace of the target, which the terminal of `yoink vscode` does not give.
