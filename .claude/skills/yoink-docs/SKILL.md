---
name: yoink-docs
description: Writing and editing the yoink documentation in `docs/content/`. Use when the user asks to write, rewrite, tighten, de-AI, audit, or add a page under `docs/content/docs/{tutorials,how-to,reference,explanation}`, or when modifying the Astro docs site in `docs/`. Enforces Simplified Technical English (pragmatic mode), the Diátaxis structure, the fixed vocabulary, the See-also convention, and the build-and-link verification.
---

# yoink-docs

The yoink docs site lives in `docs/` (Astro, hand-built, no docs theme). Pages live under `docs/content/docs/` in four Diátaxis sections: `tutorials/`, `how-to/`, `reference/`, `explanation/`. Every page ends with a `## See also` section, except the four section landing pages.

The bar for prose is **information per sentence**, written in Simplified Technical English. Load the `simple-english` skill and apply it in pragmatic mode.

## Site architecture

- **Framework**: Astro, static build to `docs/dist/`, then Pagefind indexes it. Config: `docs/astro.config.mjs`. Collections: `docs/src/content.config.ts`.
- **Docs template**: `docs/src/pages/docs/[...slug].astro`. Styles: `docs/src/styles/leaflet.css` (docs, blog, about) and `pack.css` (home page).
- **Content**: MDX and Markdown under `docs/content/docs/`. A section landing page is `<section>/index.mdx` and holds only intro text: the template lists the section's pages from the sidebar.
- **Sidebar**: `meta.json` in each section directory, with `title` and `pages`. `"---Label---"` makes a group label. A new page does not appear until it is in `meta.json`. Root order is in `docs/content/docs/meta.json`.
- **Frontmatter**: `title:` and `description:` on every page. The description is one plain sentence with no backticks.
- **Blog**: `docs/content/blog/`. **About**: `docs/content/about.mdx`.
- **Agent endpoints**: `/llms.txt`, `/llms-full.txt`, `/llms.mdx/docs/<page>/content.md` (`docs/src/pages/llms*.ts`).
- **Old URLs**: `docs/nginx.conf` redirects the pre-Diátaxis addresses. If you move a page, add a redirect there.
- **Deploy**: `cd docs && yoink up --build`. Nothing deploys from CI.

## Fixed vocabulary

| Concept | Use | Not |
|---|---|---|
| check / verify / confirm | "make sure that" | check, verify, confirm, ensure |
| the YAML settings | "config" | configuration, settings, options |
| run a command | "run" | execute, invoke, launch |
| take away a container, file or key | "remove" | delete, drop, destroy |
| put on screen | "show" | display, render, present |
| something went wrong | "error" (message), "failure" (failed operation) | issue, problem |
| the machine yoink deploys to | "host" | server, box, node, machine |
| the reader | "you" | user, developer |

"yoink" is lowercase, also at the start of a sentence. No em dashes, no semicolons, no contractions, no should/would/could/may/might.

## MDX components (global, no import)

Registered in `docs/src/components/mdx/index.ts`. Only these exist:

- `<Callout type="info|warn|error">`: use sparingly, `warn` for footguns.
- `<Cards>` with `<Card href title description />`.
- `<Tabs items={["A", "B"]}>` with `<Tab>` children.
- A heading that ends in ` [step]` becomes a numbered step. The count restarts at each `##` heading that is not a step, so each procedure starts under its own `##`.
- A `mermaid` code fence renders to SVG at build time. A diagram that fails to render fails the build.

There is no `<Files>`, no code-tab syntax, and no `!!include`.

## Verify

```sh
cd docs && pnpm build      # must pass; prints the page count
```

Then make sure that no internal link or heading anchor is broken. Heading text makes the anchor, so a renamed heading breaks inbound links: grep for the old anchor before you rename.

## Hard rules

- **Do not add facts.** A rewrite can be longer than the original when short sentences with complete grammar need more words, but every added word must restate a fact that is already on the page.
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

Each page is exactly one type. Content of another type moves to the right section and is linked.

- `tutorials/`: learning. One fixed path for a beginner, a visible result after each step, no alternatives.
- `how-to/`: one task for a competent user. Goal in the title, prerequisites, numbered steps, a way to see that it worked. No background theory.
- `reference/`: lookup. Flags, fields, schemas, example configs (`reference/examples/`). Tables and lists, no instructions, no opinions.
- `explanation/`: understanding. Why yoink works the way it does, trade-offs, comparisons. Descriptive prose, no steps, no field tables.

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

The home page (`docs/src/pages/index.astro`) embeds a terminal demo via `<Terminal src="/demo.cast" />` (`docs/src/components/Terminal.astro`). The cast file lives at `docs/public/demo.cast` and is served statically. The component loads `asciinema-player` when the recording scrolls into view.

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

1. **No facts added or lost.** Compare each changed page with `git show HEAD:<file>`. A rewrite can be longer; it cannot say more or less.
2. **Build clean**: `cd docs && pnpm build` must pass.
3. **No broken links or anchors** in `docs/dist/`.
4. **Run the banned-phrase grep** (Pass 3) and the self-check from the `simple-english` skill.

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
