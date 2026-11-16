# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

The yoink.is website is being rebuilt from scratch (decided 2026-11-15). The chosen stack is **Astro, hand-built**: MDX content collections, Pagefind search, and no docs theme, so every piece of docs chrome is designed in-house. It replaces TanStack Start + Fumadocs (`docs/`), which itself replaced a Hugo + Hextra site.

Binding delivery constraint: the site is a static build served by `nginx:alpine` and deployed with `yoink up --build` from `docs/yoink.yaml` (the site dogfoods yoink).

## Users

The primary visitor is a developer who runs a handful of services on a handful of VPS or bare-metal hosts and deploys them by hand or from CI. Side projects, prototypes, cheap experiments, and small production setups. (Revised 2026-11-17: the site first targeted agent-driven builders; the owner moved that to a secondary point.)

Other audiences that arrive and must not be turned away: developers who ship with an AI coding agent (Claude Code, Cursor, and similar) and want a deploy layer the agent can drive end to end; engineers who know Kubernetes and want its good ideas without a control plane; people outgrowing `docker compose` and shell scripts; people comparing Kamal, Coolify, Dokku, Komodo, and Uncloud.

## Product Purpose

yoink is a small, opinionated container deploy CLI + TUI. You drop a `yoink.yaml` next to your code and run `yoink up`; it connects to your hosts over SSH and drives Docker directly.

It exists because single-host PaaS tools creak at "several services, replicas, network tiers", and Kubernetes solves that at the cost of a control plane. yoink is the thinnest tool that carries the few Kubernetes ideas that pay off at small scale.

Website success: the visitor installs yoink and starts a first deploy.

## Positioning

A single binary that runs on demand, with no control plane and no agents on the hosts, whose entire control surface is a CLI plus YAML in your repo. A human at a terminal, a CI runner, and an AI coding agent all drive it the same way.

Among its peers it is the one that combines: drift detection, age-sealed secrets in the repo with no external service, hardened container defaults, pre-merge dry-run diff, no-registry standalone deploys, and port-forwarding to unpublished services.

## Operating Context

- The work happens in a terminal, an editor, a git repo, and CI. There is no web dashboard.
- The agent loop: edit `yoink.yaml` → `yoink up --dry-run --format=markdown` → `yoink up` → `yoink status` → `yoink rollback <service>` if needed. Every step is one shell command with a deterministic exit code.
- Hosts are reached by key-based SSH. Docker on the host is the only requirement.
- Three deploy modes: CI-built, local build with `yoink build --push`, and standalone `yoink up --build` with no CI and no registry.
- `yoink tui` is a k9s-style dashboard for humans: drift, logs, deploy history, rollback, shell-into.

## Capabilities and Constraints

Built in and on by default:

- age-sealed secrets (`secrets.age` committed to the repo); `provider: command` shells out to any secrets CLI
- Hardened containers: non-root uid, `cap_drop=ALL`, `no-new-privileges`, read-only rootfs, `init=tini`, tmpfs `noexec`, binds default `:ro`
- Healthcheck-gated rolling swaps, replicas behind a proxy
- Drift detection via a deterministic `spec_hash` label
- Dependency-ordered deploy waves (`depends_on`)
- Per-service network tiers
- Bundled Caddy reverse proxy: one `domain:` field gives HTTPS (Let's Encrypt or Cloudflare origin certs)
- Templates: `yoink add postgres`, `yoink add gh:owner/repo/path`
- Deploy history and rollback

Deliberately out of scope: auto-scaling, multi-tenancy / RBAC, a web UI, build pipelines.

Stated limits for agents: the TUI is for humans; a few subcommands open `$EDITOR`; `yoink up` emits human-readable progress, not structured streaming output.

Install: `brew install oddur/yoink/yoink`, `cargo install --git https://github.com/oddur/yoink yoink`, or the curl installer from GitHub releases.

Website content that must survive the rebuild: the docs content under `docs/content/` (Diátaxis structure: intro, start, guide, how-to, reference, examples, troubleshooting), the blog posts, the About page, search, `llms.txt` / `llms-full.txt` and the per-page markdown endpoints, the sitemap, and Aptabase page-view analytics.

## Brand Commitments

- Name: **yoink**, lowercase. Domain: yoink.is.
- The 🪝 hook and the name's playfulness stay. The site must not go sober and corporate.
- Voice: first person, plain, direct, honest about where other tools are the better choice. The About page is signed "Oddur".
- The primary homepage action is the install command, then the first deploy. That an AI agent can drive everything is stated, with a copyable prompt, but it is not the lead.
- No AI-hype framing. No sparkles, "AI-native", or "agentic" buzzwords. An agent is one more operator at the terminal, the same as CI.
- No enterprise or SaaS gloss. yoink is one person's MIT-licensed tool.
- No overclaiming maturity. yoink was born in 2026 and is deliberately small.

## Evidence on Hand

- `docs/public/demo.cast`: a real asciinema recording of `yoink up --build` (rolling deploy, 4 replicas).
- `docs/yoink.yaml`: the real config that deploys yoink.is itself.
- A TUI screenshot referenced in `docs/content/_index.md` (GitHub user-attachments URL).
- `docs/content/docs/intro/compared.mdx`: the comparison matrix against Kamal, Coolify, Dokku, Komodo, Uncloud, compose, and Kubernetes.
- `docs/content/docs/guide/ai-agents.md`: the agent loop, useful subcommands, and honest limitations.
- `docs/MEDIA_ROADMAP.md`: recordings and diagrams that do not exist yet.

Absent, and not to be fabricated: testimonials, customer names or logos, user counts, GitHub star counts, benchmarks, pricing.

## Product Principles

1. Files and commands are the whole interface. Anything worth doing is a `yoink` subcommand or a `yoink.yaml` field.
2. Secure and safe by default; opting out is explicit and per service.
3. Borrow only the Kubernetes ideas that pay off at a handful of hosts. Refuse the rest.
4. Say plainly when another tool fits better.
5. The site proves the product by running on it.
