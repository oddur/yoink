# Media roadmap

Tracking the docs pages that would benefit most from screen recordings, asciinema, or diagrams. Authors pick these off as bandwidth allows; nothing here blocks the doc text.

This file lives in `docs/` (not `docs/content/`) so Hugo doesn't render it.

## Asciinema (priority order)

Each line below is a high-impact "show, don't tell" spot. Asciinema is the right format when the value is *seeing the cadence* of a CLI flow — multi-step output, prompts, progress bars, a moment of "wait for it… ✓".

| Page | What to record | Why it lands |
|---|---|---|
| [`start/first-deploy.md`](content/docs/start/first-deploy.md) | `yoink up` against a fresh host: pull → create → healthcheck → swap | The literal "first time" moment. Watching the rolling deploy lands the value prop in 30s flat. |
| [`how-to/hetzner-quickstart.md`](content/docs/how-to/hetzner-quickstart.md) | The 90-second flow end-to-end (init → hcloud server create → preflight wait → up → curl) | Proves the "90 seconds" claim viscerally. Could be the homepage hero. |
| [`reference/tui.md`](content/docs/reference/tui.md) | TUI navigation tour — Dashboard → HostDetail → Logs → port-forward (`f`) → Secrets pane (`e`) | TUI keybinds in a table are dead text. Watching key overlays + pane transitions is 10× clearer. |
| [`how-to/using-templates.md`](content/docs/how-to/using-templates.md) | `yoink add postgres` interactive wizard: variable prompts → confirmation diff → seal animation | The wizard's interactive shape is the whole point; static screenshots miss the diff-then-confirm rhythm. |
| [`how-to/sealed-secrets-workflow.md`](content/docs/how-to/sealed-secrets-workflow.md) | `yoink secrets edit` round-trip: open editor, edit dotenv, save, watch reseal | Demystifies the "where do my secrets live" question in one clip. |
| [`how-to/watch-mode.md`](content/docs/how-to/watch-mode.md) | Edit Dockerfile in one pane, `yoink up --watch --build` in the other, see redeploy fire | Literally about watching things reload — recording IS the docs. |
| [`how-to/pr-comment-dry-run.md`](content/docs/how-to/pr-comment-dry-run.md) | A real PR getting the dry-run sticky comment posted by CI | Shows the "what changed" diff that operators get for free. Could be a screenshot if asciinema doesn't fit. |
| [`how-to/standalone-mode.md`](content/docs/how-to/standalone-mode.md) | `yoink up --build` showing the unregistry sidecar spin-up + per-host progress bars | The progress bars + concurrent fan-out are visual; static text doesn't convey the speed. |

## Mermaid diagrams (priority order)

Spots where the prose is fundamentally describing a graph or sequence. Mermaid renders inline in Hextra (the Hetzner quickstart already uses one).

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

## Videos (longer-form, narrated)

Different shape from asciinema — these are 3–5 min walkthroughs with voiceover, suited to the homepage and YouTube. Lower priority than the asciinema list because they require recording effort + editing.

| Page | Video | Pitch |
|---|---|---|
| Homepage / [`intro/what-and-why.md`](content/docs/intro/what-and-why.md) | "yoink in 5 minutes" | Pitch + first deploy + TUI tour. Embed at the top of `intro/what-and-why.md` and as the hero on the homepage. |
| [`reference/tui.md`](content/docs/reference/tui.md) | "TUI tour" | 3–4 min walking through every pane, narrating what each shows. Pairs with the keybind table. |
| [`how-to/cloudflare-origin-certs.md`](content/docs/how-to/cloudflare-origin-certs.md) | "Setting up Cloudflare origin-pull mTLS" | The dashboard click-path is hard to convey in screenshots; video makes the IAM-style toggles + "where's the origin cert?" obvious. |

## Conventions

- **Asciinema cast files** → `docs/static/asciinema/<page-slug>.cast` and embed via the [asciinema-player](https://github.com/asciinema/asciinema-player) shortcode (Hextra doesn't ship one; can hand-roll a partial under `docs/layouts/shortcodes/asciinema.html`).
- **Diagrams** → inline mermaid blocks (`` ```mermaid ``); see `how-to/hetzner-quickstart.md` for the existing precedent.
- **Videos** → host on YouTube, embed via Hextra's `{{< youtube >}}` shortcode.
- **Screenshots** (the cheap fallback when recording isn't worth it) → `docs/static/img/<page-slug>/<topic>.png`, embed via standard markdown.
