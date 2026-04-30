---
name: yoink-docs
description: Writing and editing the yoink documentation in `docs/content/`. Use when the user asks to write, rewrite, tighten, de-AI, audit, or add a page under `docs/content/docs/{intro,start,guide,how-to,reference,examples,troubleshooting}`, or when modifying the Hextra docs site (Hugo, mermaid, asciinema). Enforces the de-AI prose playbook (information density, no hedges, no trope phrases), the Diátaxis structure the docs follow, the See-also convention, and the per-page word-count-delta verification protocol.
---

# yoink-docs

The yoink docs site lives in `docs/` (Hugo + Hextra). Pages live under `docs/content/docs/` split by Diátaxis: `intro/`, `start/`, `guide/`, `how-to/`, `reference/`, `examples/`, plus `troubleshooting.md`. Every page ends with a `## See also` section. Most pages open with a one- or two-paragraph problem→solution framing.

The bar for prose is **information per sentence**. If a sentence can be deleted without losing technical content, delete it. Every retained sentence must carry a fact, a condition, or a reference. The voice is engineer-to-engineer: concrete, terse, opinionated where the design has an opinion.

## When to use

Trigger when the user asks to:

- Write or rewrite any page under `docs/content/docs/`.
- "De-AI", "tighten", "rewrite", or "audit" docs prose.
- Add a new how-to, recipe, example, glossary entry, reference page, or guide.
- Edit `docs/MEDIA_ROADMAP.md`.
- Touch Hugo config (`hugo.toml`), the Hextra theme, or shortcodes under `docs/layouts/`.

Do **not** trigger for unrelated docs (CLAUDE.md files, code comments, in-repo READMEs).

## Hard rules

- **Every rewrite must be shorter than the original** unless adding a `[NEEDS-SPEC]` placeholder. Per-page word-count delta is a verification gate.
- **Do not change technical content.** Anything where the meaning shifts gets flagged `[CHANGED-MEANING?]` in the commit message.
- **Do not write closing summaries.** "## What we covered" / "## Recap" / "## Summary" sections are usually deletable. Exception: `examples/*.md` "What this exercises" lists when items are concrete callouts the reader couldn't infer.
- **Do not write opening restatements.** A page titled "Reverse proxy" must not open with "This page is about the reverse proxy."
- **Do not narrate the page itself.** No "in this section we'll cover…", no "the rest of this page…" except as a hard signpost (e.g. "the rest of this page is reference").
- **Do not write standalone-bolded sentences as visual decoration.** Bold is for inline emphasis inside a sentence with other words. Glossary-shaped pages are the exception (definition pattern).

## The five-pass de-AI playbook

Apply in order on every page or section you write or rewrite. Don't conflate passes — pass 1 deletes, pass 2 rephrases, pass 3 substitutes, pass 4 restructures, pass 5 calibrates tone.

### Pass 1 — Delete fluff

A sentence is fluff if removing it loses no technical content. Categories:

- **Opening restatements.** The title already says what the page is about.
- **Closing summaries.** If the body taught the reader, the recap is a tax.
- **Transition sentences.** "Now that we've covered X, let's look at Y" — the next heading is the transition.
- **Reader-flattery / motivational framing.** "Don't worry, this is easier than it sounds." "The good news is…"
- **Meta-commentary.** "This page is the mental model for each of those…" "Each line below is a high-impact spot."
- **Standalone-bolded-phrase decoration.** A bolded phrase on its own line, not inside a sentence with other content.
- **Sentences that say nothing.** "There's a lot to unpack here." "It's worth noting that…" "Simply put…"

Example:

- **Before:** "This page covers the reverse proxy. The reverse proxy is yoink's bundled Caddy service that fronts your services with TLS."
- **After:** "Yoink bundles **Caddy** as a managed proxy service. One field exposes a service on a domain with TLS."

### Pass 2 — Replace hedges with conditions

A hedge is a vague qualifier that signals uncertainty without specifying when the qualified thing applies. The fix is a condition; if you can't write a condition, the hedge isn't load-bearing — delete.

| Hedge | Replacement |
|---|---|
| "useful for X" | "for X when Y" |
| "anything where Y" | drop OR specify the actual where |
| "typically Y" | "Y unless Z" |
| "generally Y" | "Y unless Z" |
| "in most cases" | "in the common case of Z" or delete |
| "can be used to" | "does X" / "X" |
| "might want to" | "should" or "to do X, …" |
| "tends to" | drop or specify the underlying mechanism |
| "the right format when…" | name the specific condition |

If the underlying condition isn't knowable from context, mark `[NEEDS-SPEC]` in your commit message rather than guessing.

Examples:

- **Before:** "Useful for utilities, internal admin tools, prototypes — anything where the GitHub Actions + container-registry overhead is more friction than the deploy is worth."
- **After:** "Skips the GitHub Actions + container-registry setup when you don't need it."

- **Before:** "Caddy snippets are useful for things like auth, rate limiting, and headers."
- **After:** "`caddy_extra_json:` is the escape hatch for Caddy features that aren't deploy primitives — auth, rate limiting, headers, redirects, IP allowlists, body limits."

### Pass 3 — Kill trope phrases

Direct substitutions. The trope on the left almost never carries meaning the alternative on the right doesn't.

| Trope | Use instead |
|---|---|
| leverage, utilize | use |
| robust, seamless, powerful, comprehensive | (delete; describe the property) |
| delve into | cover, explain |
| crucial, plays a crucial role | (delete or "required") |
| in the realm of | in, for |
| it's not just X, it's Y | (rewrite as the actual claim) |
| under the hood | (often deletable; or "internally") |
| out of the box | (deletable when describing zero-config) |
| simply put, essentially, fundamentally, at its core | (delete) |
| battle-tested | (delete; cite deployment scale instead) |
| first-class | (delete unless contrasting — e.g. "not yet first-class") |
| state-of-the-art, cutting-edge, game-changing, paradigm shift | (delete) |
| show, don't tell | (delete; this is meta about the docs) |
| wait for it | (delete) |
| peace of mind | (delete; describe the guarantee) |
| production-ready | (delete; describe the property) |

Run the banned-phrase grep before committing:

```sh
grep -nE "leverage|utilize|seamless|robust|powerful|comprehensive|delve|crucial|in the realm|it's not just|under the hood|out of the box|essentially|fundamentally|simply put|at its core|battle-tested|first-class|world-class|state-of-the-art|cutting-edge|game-chang|paradigm" docs/content/ -r
```

Second pass for hedges and tropes the first grep doesn't catch:

```sh
grep -nE "the right (format|tool|fit) when|anything where|peace of mind|production-ready|a few rough edges|exactly the (right|kind)|happens to be|truly|merely" docs/content/ -r
```

False positives are real. "First-class" describing a roadmap gap ("not yet first-class") is meaningful. "Out of the box" describing actual zero-config defaults is appropriate. The bar: does the phrase carry meaning, or is it filler?

### Pass 4 — Structural fixes

- **Tricolons.** "X, Y, and Z — anything where W" almost always disguises a missing condition. Rewrite as a list with concrete items (drop the "anything where") or one specific category.
- **Parallel paragraph padding.** Three paragraphs of "X does A. Y does B. Z does C." can usually be a table.
- **Bullets vs prose.** Bullets when items are independent; prose when items connect logically. Don't bullet a single sentence broken at commas. Don't prose a 2-column lookup.
- **Headers that exist only to break up text.** If two adjacent sections are one paragraph each, fold them.
- **Tables for `field → meaning`.** Reference pages and glossaries should be tables when the shape is `name | type | default | notes`. The yoink config reference is the canonical example.

Lists that should be sentences:

- **Before:**
  ```
  - Routes the hostname.
  - Issues a Let's Encrypt cert.
  - Sets up auto-renewal.
  - Rolls cleanly with the service.
  ```
- **After:** "Routes the hostname, issues a Let's Encrypt cert, auto-renews, and rolls with the service it fronts."

Sentences that should be lists: a run-on sentence with four conditions becomes a numbered list.

### Pass 5 — Tone calibration

After passes 1–4, read the page. Catch:

- **Enthusiasm.** "Awesome", "great", "amazing", exclamation points outside error-output examples.
- **Reader-coaching.** "Don't be afraid to…", "It's totally fine to…"
- **Performative balance.** "On the other hand, some teams prefer…" when the page has already taken a position. Either commit or remove.
- **Apologetic preambles.** "This is a quick note about…", "I just want to mention…"

Direct statements only.

## Self-check after the five passes

Before committing, ask:

1. Could a reader skip any sentence and lose nothing?
2. Did I replace vagueness with specificity, or just with different vagueness?
3. Is the rewrite shorter than the original?

If any answer fails, the page isn't done.

## Page structure (Diátaxis)

The split is load-bearing. Don't put a tutorial section inside a reference page; don't put a reference table inside a how-to.

- `intro/` — pitch, comparison. What yoink is and why.
- `start/` — getting started. Tutorials. Linear, one-shot.
- `guide/` — explanation. Mental model. "How it works", "why this exists".
- `how-to/` — task-oriented. "How to do X". Each is self-contained.
- `reference/` — lookup. `cli.md`, `config.md`, `tui.md`, `glossary.md`. Mostly tables; minimal prose.
- `examples/` — annotated end-to-end configs. `hobby-tool.md`, `polyglot-stack.md`, `production.md`.

## Narrative openers

Pages that need framing (most `guide/`, `examples/`, and `how-to/`) open with a problem→solution paragraph:

> *Sentence 1: the friction or shape of the problem.*
> *Sentence 2: how yoink addresses it, with the load-bearing field/flag/concept named.*

Reference pages, glossary entries, and lookup-shaped pages skip the opener and start with the table or first definition.

Marketing-shaped openers to avoid:

- "Putting an app behind HTTPS used to be a 30-line nginx config plus…" — reads like ad copy.
- "Most real apps aren't one container — they're a backend, a frontend, a cache…" — tricolon list followed by hedge.
- "The deploy tools most operators reach for first share a few rough edges." — "a few rough edges" is a hedge.

The fix in each case: name the actual problem mechanically, then name the yoink answer.

## See-also convention

Every page ends with `## See also` listing 2–4 cross-links with one-line hooks. Format:

```
## See also

- [Title](/docs/path) — one-line hook.
- [Title](/docs/path) — one-line hook.
```

The hook is what the linked page covers, not why the reader should click. Concrete, not promotional.

## Cross-link conventions

- Link to other docs pages by absolute path: `/docs/guide/proxy`, not `../guide/proxy.md`.
- First reference of a concept gets the link. Subsequent references in the same page don't repeat it.
- See-also links at the bottom always link; the body link is for in-flow context.

## Code-fence and inline-code conventions

- Inline-code anything that is literal text the user types or yoink reads: field names (`services:`), flags (`--build`), file paths (`secrets.age`), env var names (`YOINK_AGE_KEY`), commands (`yoink up`).
- Don't inline-code prose words that happen to be jargon. "service" is prose; `services:` is a field.
- Code fences get a language tag. `yaml`, `sh`, `rust`, `text` for output.
- Comments inside YAML examples explain *why* a field is set, not *what* it is. Don't write `# the api service` next to `name: api`.

## Callouts

Use `{{< callout type="..." >}}` sparingly:

- `info` — adjacent context that's optional reading.
- `warning` — a footgun that bites without warning. Use when the wrong action looks like the right action.
- `tip` — a non-obvious shortcut or affordance. Rare.

If a callout is just emphasis on a paragraph that should have been emphasized inline, delete the callout and rewrite the paragraph.

## Em-dashes, commas, parens

- Em-dash (`—`) for parenthetical asides that interrupt the sentence's main flow.
- Commas for additions that flow with the sentence.
- Parens for asides the reader can skip without losing the sentence.

Don't stack three em-dashes in one paragraph. Don't use the en-dash (`–`); the docs use em-dashes.

## Verification protocol

Before declaring a docs change done:

1. **Per-page word-count delta is negative** (unless `[NEEDS-SPEC]` placeholder added). Compute with:
   ```sh
   for f in <changed files>; do
     before=$(git show HEAD:"$f" | wc -w | tr -d ' ')
     after=$(wc -w < "$f" | tr -d ' ')
     printf "%-50s before=%s after=%s delta=%s\n" "$f" "$before" "$after" "$((after - before))"
   done
   ```
2. **Hugo build clean.** `cd docs && hugo --gc`. The two pre-existing Hextra warnings (`.Site.Data` deprecation, `tabs` shortcode `items` parameter) are expected; no new errors.
3. **No broken cross-links.** `grep -roE '\(/docs/[^)]+\)' docs/content/` then resolve each path.
4. **Run the banned-phrase grep** (see Pass 3).
5. **Spot-check the rewrite against the three self-check questions** above.

## Commit protocol

One commit per logical group of pages, not per file. Commit message carries the audit trail:

```
docs: <one-line summary>

<context: what was changed and why>

Per-page word delta (all negative):
  path/to/file.md          -123
  path/to/other.md          -45
  total                    -168

[NEEDS-SPEC]: <list of places a hedge was removed but the underlying condition wasn't knowable>
[CHANGED-MEANING?]: <list of rewrites where meaning preservation is <90% certain>
```

If `[NEEDS-SPEC]` or `[CHANGED-MEANING?]` are empty, write `none.` rather than omitting the line.

## When to push back

If the user asks for prose that violates the playbook (a marketing-shaped opener for a reference page, a closing summary that adds nothing, a hedge instead of a condition), explain the tradeoff and propose the playbook-conformant version. Don't silently apply the playbook against an explicit request — flag it.
