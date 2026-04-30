# Media roadmap

Docs pages that need a recording or diagram. Pick off as bandwidth allows; not blocking.

Lives in `docs/` (not `docs/content/`) so Hugo doesn't render it.

## Asciinema (priority order)

| Page | What to record | Why |
|---|---|---|
| [`start/first-deploy.md`](content/docs/start/first-deploy.md) | `yoink up` against a fresh host: pull → create → healthcheck → swap | The first-deploy moment in 30s. |
| [`how-to/hetzner-quickstart.md`](content/docs/how-to/hetzner-quickstart.md) | End-to-end: init → hcloud server create → preflight wait → up → curl | Proves the 90-second claim. Candidate homepage hero. |
| [`reference/tui.md`](content/docs/reference/tui.md) | Dashboard → HostDetail → Logs → port-forward (`f`) → Secrets pane (`e`) | Keybind tables don't show pane transitions. |
| [`how-to/using-templates.md`](content/docs/how-to/using-templates.md) | `yoink add postgres` wizard: variable prompts → confirmation diff → seal | Static screenshots miss the diff-then-confirm rhythm. |
| [`how-to/sealed-secrets-workflow.md`](content/docs/how-to/sealed-secrets-workflow.md) | `yoink secrets edit` round-trip: open editor, edit dotenv, save, reseal | Shows where secrets live. |
| [`how-to/watch-mode.md`](content/docs/how-to/watch-mode.md) | Edit Dockerfile in one pane, `yoink up --watch --build` in the other | Recording is the feature. |
| [`how-to/pr-comment-dry-run.md`](content/docs/how-to/pr-comment-dry-run.md) | A real PR getting the dry-run sticky comment from CI | Screenshot works if asciinema doesn't fit. |
| [`how-to/standalone-mode.md`](content/docs/how-to/standalone-mode.md) | `yoink up --build` with unregistry sidecar spin-up + per-host progress bars | Concurrent fan-out is visual. |

## Mermaid diagrams (priority order)

Where the prose is describing a graph or sequence. Mermaid renders inline in Hextra (the Hetzner quickstart uses one).

| Page | Diagram | What it shows |
|---|---|---|
| [`guide/architecture.md`](content/docs/guide/architecture.md) | `flowchart` — drift detection cycle | Container labelled with `spec_hash` → reconcile compares vs config → start new → healthcheck → swap → reap old. Currently a numbered list at lines 130–135. |
| [`guide/architecture.md`](content/docs/guide/architecture.md) | `sequenceDiagram` — deploy lock + healthcheck-gated swap across replicas | Operator → host lock → per-replica wave → healthcheck → swap. Makes the "capacity stays at N-1" property visible. |
| [`guide/proxy.md`](content/docs/guide/proxy.md) | `flowchart` — request handler chain | Client → Caddy `:443` → match domain → handler chain (auth → rate limit → reverse_proxy → upstream container). Currently described in prose under "Per service" + "Per-route handler order". |
| [`guide/networking.md`](content/docs/guide/networking.md) | `flowchart` — port-forward sidecar architecture | Operator laptop ←SSH tunnel→ host ←docker network→ alpine/socat sidecar ←container DNS→ target service. The "What it works on" section currently lists shapes but doesn't picture them. |
| [`guide/networking.md`](content/docs/guide/networking.md) | `flowchart` — multi-host service distribution | One service config × N hosts → containers, plus Caddy as the front. Currently described under "How `yoink up` schedules across hosts". |
| [`guide/secrets.md`](content/docs/guide/secrets.md) | `flowchart` — sealing/unsealing flow | Operator edits → `yoink secrets edit` → decrypt with identity → editor → reseal against recipients → write. Loops with the runtime: deploy → unseal → inject env vars. |
| [`guide/secrets.md`](content/docs/guide/secrets.md) | `flowchart` — identity resolution lookup order | YOINK_AGE_KEY → YOINK_AGE_KEY_FILE → keys-dir scan → legacy path. The table at line 50 reads bottom-to-top in the operator's mental model; a flowchart inverts that into top-down. |
| [`guide/deploy-modes.md`](content/docs/guide/deploy-modes.md) | `flowchart` — build origin × distribution decision | "Where am I building?" → "Where does the image go?" → matrix cell. The 2×2 table works but a flowchart helps newcomers pick. |

## Videos (3–5 min, narrated)

Lower priority than asciinema — more recording + editing effort.

| Page | Video | Pitch |
|---|---|---|
| Homepage / [`intro/what-and-why.md`](content/docs/intro/what-and-why.md) | "yoink in 5 minutes" | Pitch + first deploy + TUI tour. Hero embed. |
| [`reference/tui.md`](content/docs/reference/tui.md) | "TUI tour" | Walk every pane, narrate what each shows. |
| [`how-to/cloudflare-origin-certs.md`](content/docs/how-to/cloudflare-origin-certs.md) | "Cloudflare origin-pull mTLS" | The dashboard click-path is hard to screenshot. |

## Conventions

- **Asciinema casts** → `docs/static/asciinema/<page-slug>.cast`, embed via [asciinema-player](https://github.com/asciinema/asciinema-player) (Hextra doesn't ship a shortcode; hand-roll under `docs/layouts/shortcodes/asciinema.html`).
- **Diagrams** → inline mermaid blocks; see `how-to/hetzner-quickstart.md`.
- **Videos** → YouTube, embed via Hextra's `{{< youtube >}}`.
- **Screenshots** → `docs/static/img/<page-slug>/<topic>.png`, standard markdown embed.
