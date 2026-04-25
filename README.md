# yoink

A small, opinionated container deploy tool for people who run a handful of services on a handful of hosts.

## What it is

`yoink` deploys Docker containers to remote hosts and does the rolling-container-swap loop with zero downtime: pull the new image, start the new container, wait for it to be healthy, then retire the old one. That's it.

It is not Kubernetes. It is not Nomad. It is not trying to be Kamal. It is the smallest tool that solves "deploy N services across N hosts without breaking traffic."

## What it is not

- Not a reverse proxy. Use Caddy with `caddy-docker-proxy`.
- Not a secret store. Pulls from Infisical at runtime; never stores secrets itself.
- Not a service for stateful accessories. Use Docker Compose on the host for Redis, Postgres, etc.
- Not a load balancer. Cloudflare or whatever you already have sits in front.
- Not multi-tenant, not cluster-aware, not HA. One operator, one config, one deploy at a time.

## Talking to Docker on remote hosts

`yoink` never installs an agent, daemon, or any code on the target hosts. It drives Docker on each host using Docker's native `ssh://` transport: `bollard::Docker::connect_with_ssh("ssh://<user>@<host>", ...)` opens an SSH tunnel and speaks the Docker Engine API over the remote daemon's Unix socket. yoink is a single static Rust binary that runs on the operator's laptop or on CI; nothing runs on the hosts.

The remote daemon is reached through `/var/run/docker.sock`. The SSH user must be able to read that socket — in practice this means the user is in the `docker` group (or is root). yoink does not expose the Docker daemon on a TCP port, set up `ssh -L` forwards, or generate TLS certs. The native `ssh://` transport handles everything.

To verify a host is reachable:

```
yoink preflight --config yoink.toml
```

This runs `Docker::version()` against each configured host and prints the server version, API version, and platform — or the error if the host is unreachable. If preflight fails, the problem is SSH auth, docker group membership, or the daemon not running — in that order of likelihood.

> **Important.** Membership in the `docker` group is functionally equivalent to passwordless root on the host. The deploy user's SSH key should be treated as a root credential and scoped accordingly via Tailscale ACLs.

## Architecture

```
laptop / CI:               yoink CLI (Rust binary; tokio + bollard)
                              |
                              | bollard ssh:// transport (Tailscale SSH)
                              v
host-a, host-b, ...:       /var/run/docker.sock (Docker Engine API)
                              |
                              v
                           caddy-docker-proxy --> app containers
                                                  (labeled for routing)

separate concern:          docker compose on each host -->
                           redis, postgres, caddy itself
```

- **Tailscale** provides connectivity, hostnames (MagicDNS), and SSH auth.
- **Cloudflare** sits in front of the public hosts, handles LB and edge TLS.
- **Caddy + caddy-docker-proxy** runs on each host, watches Docker labels, routes to local containers.
- **Compose** owns the long-lived stateful services (Redis, Postgres, Caddy itself).
- **yoink** owns the short-lived stateless app services. Nothing else.

## Concepts

**Service.** One logical app. Has an image, a healthcheck, optional secrets, and a list of hosts to run on.

**Host.** A remote machine reachable over Tailscale SSH with Docker installed and the deploy user in the `docker` group.

**Deploy.** Pulling a new image tag for a service and rolling it out across its hosts.

**Strategy.** How a deploy iterates hosts: `parallel` (all at once) or `rolling` (one at a time, gated on health). Currently a single host is supported.

State of the world is queried from Docker, not stored locally. Containers are labeled `yoink.service=<name>` and `yoink.version=<sha>`; that is the source of truth.

## Configuration

One TOML file per service, by convention checked into the service's repo as `yoink.toml`. See [`examples/yoink.toml`](examples/yoink.toml).

`[run.options]` is a typed mirror of the docker-run flags yoink supports — `memory`, `cap_drop`, `cap_add`, `security_opt`, `read_only`, `tmpfs`, `network_aliases`, `restart`. Mapped directly to bollard's `HostConfig`. Anything not in this list is unsupported; either set it on the image's Dockerfile or move the workload to a docker-compose accessory.

## CLI

```
yoink preflight                   verify Docker is reachable on each host
yoink deploy [--tag <sha>]        deploy a new image tag
yoink status                      show what's running where
yoink rollback                    re-deploy the previous version
yoink tui [--mode dashboard|logs] interactive ratatui dashboard / live logs
```

All commands take `--config <path>` (default: `./yoink.toml`). Use `-v` / `--verbose` for structured logs to stderr.

## Build & test

```bash
task yoink:check       # cargo check --workspace
task yoink:test        # unit + integration tests
task yoink:clippy      # clippy with -D warnings
task yoink:run -- ...  # cargo run with args
```
