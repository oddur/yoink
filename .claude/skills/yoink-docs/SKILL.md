---
name: yoink-docs
description: Writing and editing the yoink documentation in `docs/content/`. Use when the user asks to write, rewrite, tighten, de-AI, audit, or add a page under `docs/content/docs/{intro,start,guide,how-to,reference,examples,troubleshooting}`, or when modifying the Fumadocs + TanStack Start docs site. Enforces the de-AI prose playbook (information density, no hedges, no trope phrases), the Diátaxis structure the docs follow, the See-also convention, and the per-page word-count-delta verification protocol.
---

# yoink-docs

The yoink docs site lives in `docs/` (Fumadocs + TanStack Start SPA). Pages live under `docs/content/docs/` split by Diátaxis: `intro/`, `start/`, `guide/`, `how-to/`, `reference/`, `examples/`, plus `troubleshooting.md`. Every page ends with a `## See also` section. Most pages open with a one- or two-paragraph problem→solution framing.

The bar for prose is **information per sentence**. If a sentence can be deleted without losing technical content, delete it. Every retained sentence must carry a fact, a condition, or a reference. The voice is engineer-to-engineer: concrete, terse, opinionated where the design has an opinion.

## Site architecture

- **Framework**: Fumadocs on TanStack Start, statically prerendered via Vite → `.output/public/`
- **Content**: MDX + Markdown under `docs/content/docs/`. Section index pages use `index.mdx` (not `_index.md`).
- **Sidebar ordering**: `meta.json` in each directory (replaces Hugo `weight:` frontmatter). Every section has a `meta.json` with `title`, `icon`, and `pages` array.
- **Frontmatter required fields**: `title:` and `description:` on every page. `weight:` is ignored by Fumadocs — do not use it.
- **Blog**: `docs/content/blog/` — MDX posts with `title`, `date`, `description` frontmatter.
- **About**: `docs/content/about.mdx` — single page at `/about/`.
- **Deploy**: `cd docs && yoink up --build` — yoink derives the image tag from the Docker build digest automatically (no `tag:` in `yoink.yaml`).

## When to use

Trigger when the user asks to:

- Write or rewrite any page under `docs/content/docs/`.
- "De-AI", "tighten", "rewrite", or "audit" docs prose.
- Add a new how-to, recipe, example, glossary entry, reference page, or guide.
- Edit `docs/MEDIA_ROADMAP.md`.
- Touch Fumadocs config (`source.config.ts`, `src/lib/layout.shared.tsx`, `meta.json` files).
- Add or modify MDX components in `docs/src/components/mdx.tsx`.

Do **not** trigger for unrelated docs (CLAUDE.md files, code comments, in-repo READMEs).

## Available MDX components (global — no import needed)

All registered in `docs/src/components/mdx.tsx`:

### Callout
```mdx
<Callout type="info">Adjacent context that is optional reading.</Callout>
<Callout type="warn">A footgun that bites without warning.</Callout>
<Callout type="error">Something that will break.</Callout>
```
Use sparingly. `warn` for footguns only. If the callout is emphasis on a paragraph that should be emphasized inline, delete it and rewrite the paragraph.

### Cards
```mdx
<Cards>
  <Card href="/docs/guide/architecture" title="How it works" description="Drift detection, deploy lock, spec_hash." />
  <Card href="/docs/guide/secrets" title="Secrets" description="AGE-sealed in repo, plus provider:command." />
</Cards>
```
Used in section index pages to surface subsections.

### Tabs (explicit JSX)
```mdx
<Tabs items={["Option A", "Option B"]}>
  <Tab>Content for A</Tab>
  <Tab>Content for B</Tab>
</Tabs>
```

### Code tabs (markdown syntax — prefer over JSX Tabs for code groups)
````mdx
```bash tab="Homebrew"
brew install oddur/yoink/yoink
```
```bash tab="Cargo"
cargo install --git https://github.com/oddur/yoink yoink
```
````

### Steps (via remarkSteps — no JSX wrapper needed)
```md
### Generate the keypair [step]

```sh
yoink secrets key generate
```

### Add the recipient to yoink.yaml [step]

...
```
The `[step]` tag on a heading auto-wraps it in a numbered step. No `<Steps>` JSX needed. All existing how-to files use this pattern.

### File tree
```mdx
<Files>
  <Folder name="docs" defaultOpen>
    <File name="yoink.yaml" />
    <File name="secrets.age" />
  </Folder>
</Files>
```

### Mermaid diagrams (fenced code block — no component)
````md
```mermaid
flowchart TB
  A --> B
```
````
Rendered at build time via `beautiful-mermaid` + `remarkMdxMermaid`. Images are click-to-zoom automatically.

## remarkInclude

Include content from other files:
```md
!!include ./snippets/prereqs.md
```
Put shared snippets in `docs/content/docs/_snippets/` (underscore prefix keeps them out of the page tree).

## meta.json format

```json
{
  "title": "How-to",
  "icon": "Wrench",
  "pages": [
    "---Quickstarts---",
    "hetzner-quickstart",
    "tanstack-stack",
    "---Secrets & Security---",
    "sealed-secrets-workflow"
  ]
}
```

- `"---Label---"` — sidebar separator with label
- `"[Text](url)"` — external link entry in sidebar
- `"..."` — remaining pages alphabetically
- `"!page"` — exclude from rest

Section icons (Lucide names): intro→BookOpen, start→Rocket, guide→Map, how-to→Wrench, reference→BookMarked, examples→LayoutTemplate, troubleshooting→CircleAlert.

## Hard rules

- **Every rewrite must be shorter than the original** unless adding a `[NEEDS-SPEC]` placeholder.
- **Do not change technical content.** Anything where the meaning shifts gets flagged `[CHANGED-MEANING?]` in the commit message.
- **Do not write closing summaries.** "## What we covered" sections are deletable. Exception: `examples/*.md` "What this exercises" lists when items are concrete callouts the reader could not infer.
- **Do not write opening restatements.** A page titled "Reverse proxy" must not open with "This page is about the reverse proxy."
- **Do not narrate the page itself.** No "in this section we will cover…"
- **Do not write standalone-bolded sentences as visual decoration.**

## The five-pass de-AI playbook

### Pass 1 — Delete fluff

Remove: opening restatements, closing summaries, transition sentences, reader-flattery, meta-commentary, standalone-bolded-phrase decoration, sentences that say nothing ("It's worth noting that…").

### Pass 2 — Replace hedges with conditions

| Hedge | Replacement |
|---|---|
| "useful for X" | "for X when Y" |
| "typically Y" | "Y unless Z" |
| "generally Y" | "Y unless Z" |
| "in most cases" | "in the common case of Z" or delete |
| "can be used to" | "does X" |
| "might want to" | "should" or "to do X, …" |

### Pass 3 — Kill trope phrases

| Trope | Use instead |
|---|---|
| leverage, utilize | use |
| robust, seamless, powerful, comprehensive | (delete; describe the property) |
| delve into | cover, explain |
| crucial, plays a crucial role | (delete or "required") |
| out of the box | (deletable when describing zero-config) |
| simply put, essentially, fundamentally, at its core | (delete) |
| battle-tested, first-class, production-ready | (delete; describe the property) |

Banned-phrase grep:
```sh
grep -nE "leverage|utilize|seamless|robust|powerful|comprehensive|delve|crucial|in the realm|it's not just|under the hood|out of the box|essentially|fundamentally|simply put|at its core|battle-tested|first-class|world-class|state-of-the-art|cutting-edge" docs/content/ -r
```

### Pass 4 — Structural fixes

- **Tricolons** disguising a missing condition → rewrite as a list with concrete items.
- **Parallel paragraph padding** → table.
- **Bullets vs prose** — bullets when items are independent; prose when they connect logically.
- **Tables for field → meaning** — reference pages use `name | type | default | notes`.

### Pass 5 — Tone calibration

Catch: enthusiasm, reader-coaching ("Don't be afraid to…"), performative balance, apologetic preambles.

## Page structure (Diátaxis)

- `intro/` — pitch, comparison. What yoink is and why.
- `start/` — getting started. Tutorials. Linear, one-shot.
- `guide/` — explanation. Mental model. "How it works", "why this exists".
- `how-to/` — task-oriented. "How to do X". Each is self-contained.
- `reference/` — lookup. `cli.md`, `config.md`, `tui.md`, `glossary.md`. Mostly tables; minimal prose.
- `examples/` — annotated end-to-end configs. `hobby-tool.md`, `polyglot-stack.md`, `production.md`.

## Narrative openers

Pages that need framing open with:

> *Sentence 1: the friction or shape of the problem.*
> *Sentence 2: how yoink addresses it, with the load-bearing field/flag/concept named.*

Reference pages and lookup-shaped pages skip the opener and start with the table or first definition.

## See-also convention

Every page ends with `## See also` listing 2–4 cross-links:

```
## See also

- [Title](/docs/path) — one-line hook.
- [Title](/docs/path) — one-line hook.
```

## Code-fence and inline-code conventions

- Inline-code anything literal: field names (`services:`), flags (`--build`), file paths (`secrets.age`), commands (`yoink up`).
- Code fences always get a language tag: `yaml`, `sh`, `rust`, `text` for output, `dotenv` (not `env`) for env files.
- Comments inside YAML examples explain *why* a field is set, not *what* it is.

## Terminal recordings (asciinema)

The home page (`docs/src/routes/index.tsx`) embeds a terminal demo via `<TerminalPlayer src="/demo.cast" />`. The cast file lives at `docs/public/demo.cast` and is served statically. The player component (`docs/src/components/terminal-player.tsx`) lazy-loads `asciinema-player` after mount to avoid SSR issues.

### Recording

```sh
brew install asciinema   # one-time
cd docs
asciinema rec public/demo.cast --cols 100 --rows 18 --command "yoink up --build --tag docs=$(git rev-parse --short HEAD)"
```

Record in headless mode (no interactive TTY needed — the Bash tool can run this). Aim to capture: the build announcement line, shipping via unregistry, and the rolling healthcheck deploy. Docker build output can be cut afterward.

### Cast file format

The file uses **asciinema v3** format: JSON header on line 1, one event per subsequent line. Each event: `[relative_seconds, type, data]` where type is `"o"` (output) or `"x"` (exit). Timestamps are **relative to the previous event**, not absolute from start.

### Editing the cast

**Remove docker build noise** — keep only the build announcement line + shipping/deploy events:
```python
with open('docs/public/demo.cast') as f:
    lines = f.readlines()
header = lines[0]
events = [json.loads(l) for l in lines[1:] if l.strip()]
# Keep: events[0] (build announcement), then skip build churn, resume at shipping
kept = [events[0]] + [[1.5, "o", "\r\n"]] + events[shipping_start_idx:]
with open('docs/public/demo.cast', 'w') as f:
    f.write(header)
    for e in kept: f.write(json.dumps(e) + '\n')
```

**Mask private IPs** — replace real host IPs with RFC 5737 documentation addresses:
```python
content = content.replace('actual.ip.here', '203.0.113.10')
```

**Remove unwanted lines** (WARN, error noise):
```python
lines = [l for l in lines if 'WARN' not in l and 'ERROR' not in l]
```

**Add a pause at the end** — insert a blank output event BEFORE the exit event (`"x"`), not after it (the player stops at the exit event):
```python
exit_idx = next(i for i, e in enumerate(events) if e[1] == 'x')
events.insert(exit_idx, [10.0, "o", ""])
```

**Watch out for double-encoding** — if you read lines with `f.readlines()` and then write them with `json.dumps(line_string)`, the string gets double-encoded. Write kept lines raw; only use `json.dumps` for synthetic events you construct as Python lists. To fix an already-double-encoded file:
```python
parsed = json.loads(line)
if isinstance(parsed, str):
    parsed = json.loads(parsed)  # decode the inner array
```

### Verifying the cast

```sh
# Check event count and structure
python3 -c "
import json
with open('docs/public/demo.cast') as f: lines = f.readlines()
events = [json.loads(l) for l in lines[1:] if l.strip()]
print(f'{len(events)} events')
for e in events[:3] + events[-3:]: print(e[1], e[0], repr(e[2][:60]))
"

# Verify no real IPs or WARN lines remain
grep -c "WARN\|your.real.ip" docs/public/demo.cast || echo "clean"
```

## Verification protocol

1. **Per-page word-count delta is negative**:
   ```sh
   for f in <changed files>; do
     before=$(git show HEAD:"$f" | wc -w | tr -d ' ')
     after=$(wc -w < "$f" | tr -d ' ')
     printf "%-50s before=%s after=%s delta=%s\n" "$f" "$before" "$after" "$((after - before))"
   done
   ```
2. **Build clean**: `cd docs && pnpm build` — must show `Prerendered 143 pages:` with no errors.
3. **No broken cross-links**: `grep -roE '\(/docs/[^)]+\)' docs/content/` then resolve each path.
4. **Run the banned-phrase grep** (Pass 3).
5. **Self-check**: Could a reader skip any sentence and lose nothing? Did I replace vagueness with specificity? Is the rewrite shorter?

## Commit protocol

```
docs: <one-line summary>

<context: what was changed and why>

Per-page word delta (all negative):
  path/to/file.md          -123
  total                    -123

[NEEDS-SPEC]: none.
[CHANGED-MEANING?]: none.
```
