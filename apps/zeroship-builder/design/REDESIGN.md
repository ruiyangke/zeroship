# zeroship-builder — Atelier redesign

A creator's workshop, not a developer's terminal.

## Audience reset

Per `/home/ruiyang/Projects/appbase/.claude/worktrees/kernel-cut/CLAUDE.md`:

> A platform where anyone can create, launch, and monetize software — without
> writing code. Creators describe what they want. AI builds it. Think Shopify
> for AI-generated apps.

Today's UI **targets developers** — monospace prose, lowercase chrome,
neon-green-on-pure-black, dense iconography, `// ai builder` comment-syntax
decoration. That brand strongly signals "tool for hackers." It mismatches the
stated audience (small-business owners with an idea and no terminal skills).

The redesign rebrands away from "developer hacker" toward "patient craftsperson."

## Aesthetic direction — Atelier

Think: a high-end tailor's room. A ceramicist's bench. The lobby of a small
indie publisher. **Warmth, paper, ink, considered typography.**

Not: futuristic, neon, dark, dense, monospace.

Why: every other AI builder (Bolt, Lovable, v0, Replit Agent) plays in the
"bright cheerful purple-pink modern" register. Going *atelier* is a visible
positioning move — "this is the careful one." It also signals that the AI
isn't slop-generating output; it's drafting, considering, shipping.

## System

| Token | Value | Use |
|---|---|---|
| `--paper` | `oklch(0.97 0.012 89)` | page background |
| `--ink` | `oklch(0.18 0.013 60)` | primary text |
| `--ink-soft` | `oklch(0.36 0.013 65)` | secondary text |
| `--pencil` | `oklch(0.55 0.012 70)` | tertiary, hints |
| `--rule` | `oklch(0.78 0.014 75)` | hairline rules, borders |
| `--tomato` | `oklch(0.61 0.21 27)` | single sharp accent — actions, "live" state |
| `--clay` | `oklch(0.84 0.04 60)` | secondary surfaces |

**Type:** `Fraunces` (display + body — bold move) + `Public Sans` (UI chrome
only). Both Google Fonts, free. Fraunces' optical sizing + characterful
italic does most of the work. No Inter, no Space Grotesk, no Geist — those are
the AI-tool defaults we're avoiding.

**Layout:** asymmetric editorial grid — wide left margin for marginalia
(volume number, studio hours, est-stamp), 760px content column, generous
right gutter. On <1100px, marginalia hides and the content column centers.

**Motion:** stagger-reveal on first paint (lines fade up at 60ms intervals),
ink-underline draws under the prompt textarea on focus, paper-fold corner on
project cards expands on hover, stamp button presses with rotation+shadow.

## Twelve UX changes the redesign delivers

1. **Identity at the door.** `zeroship.` wordmark in Fraunces italic, single
   tomato dot. Replaces the all-caps tracking-wider monospace logo.
2. **Hero with a verb.** "What will you **make**?" — italicized, swashed,
   highlighted with a soft tomato underline-mark. Action-led, warm.
3. **Notebook prompt.** Textarea looks like a steno pad — red ruler on left,
   paper background, ink-underline draws on focus, italic placeholder.
   Replaces the sterile bordered textarea.
4. **A real lede.** "Every project starts with a sentence… usually under a
   minute." Sets time expectation + tone.
5. **Inspiration chips, italicized & quoted.** Real example prompts, click to
   populate the textarea. Replaces the monospace bulleted "examples" list in
   the empty state.
6. **Project cards as letterheads.** Project number in tomato italic
   (`№ 04`), large serif title, italic prompt as tagline, paper-fold corner
   that grows on hover, "Live" indicator in tomato with a pulsing micro-dot.
   Replaces the dot+plan+timestamp git-metadata cards.
7. **Live banner — the celebration moment.** Inky black banner shouting
   "Supper Society is *live*" with a copyable URL pill, "add custom domain,"
   "share on twitter," "add a price" actions. **The deploy moment is now a
   product event, not an iframe reload.**
8. **Marginalia.** Volume number, edition, studio hours, an `est. 2026`
   stamp at -7° rotation. Costs nothing, communicates "considered indie
   publisher" instantly.
9. **Hairline rules.** Replace heavy `border-border` boxes with single
   `0.78` luminance hairlines. Editorial feel.
10. **Single accent, used sparingly.** Tomato shows up only in places that
    earn it: the verb in the hero, the live indicator, the URL pill border,
    the stamp button. Quiet rest of the page.
11. **Paper grain.** A faint SVG-noise overlay at 0.5 opacity, multiply blend.
    Almost subliminal — but the page reads as printed paper, not a screen.
12. **No mono in body.** Mono survives only where it's meaningful (URL pills).
    Prose, metadata, button labels — all sans or serif.

## What the redesign explicitly does NOT include yet

- Workspace shell (chat + preview + tabs) — Phase 2
- Files / logs / env tabs — should be hidden behind a "developer drawer"
  toggle, only visible when explicitly requested. Phase 2.
- Auth pages — restyle once Phase 1 system is locked.
- Custom domain / payments wizards — these belong to the live banner's
  follow-on flow, Phase 3.

## How to view

```bash
# from repo root
open apps/zeroship-builder/design/redesign-preview.html
# or:
xdg-open apps/zeroship-builder/design/redesign-preview.html
# or just drag the file into a browser
```

It's a single self-contained HTML file. Loads Fraunces + Public Sans from
Google Fonts. No build step. No dependencies.

## What this is NOT yet

A spec. Once we agree on direction, the next step is to replace the React
components in `src/client/pages/Home.tsx`, `src/client/workspace/*`, and the
shadcn primitives in `src/client/components/ui/*` with the new tokens. The
HTML preview is faster to iterate on look-and-feel; React migration is a
mechanical port once the system is settled.
