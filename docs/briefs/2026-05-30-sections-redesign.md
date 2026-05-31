# Sections redesign — "Foundation + targeted polish" (premium pass)

Two harsh visual critiques (orchestrator + codex) of the 7 sections/ bands converged:
the set reads like a clean Tailwind starter, not a $$$ product — **timid type scale,
everything centered on one monotone gradient (no page rhythm), no depth, accent used
as button-paint, under-scaled stats, forgettable CTA, weak eyebrows, FAQ open-state
looks like a debug outline, footer feels bare**. User picked "Foundation + targeted
polish": ship the high-leverage backbone (marketing display type scale + section
tone/surface system) then per-section upgrades. No novel dark-theme/gradient-mesh
language beyond what tokens support (accent-fill via --zs-accent + --zs-accent-ink
IS supported; use it for the contrast band).

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`
(cwd INSIDE — nested-pnpm gotcha). Commit-only, NEVER push.

## Existing tokens to build on (do NOT duplicate/conflict)
Type scale tops at `--zs-text-large-title-*` (34px/700/-0.022em). Surfaces:
`--zs-surface` (near-white tint), `--zs-surface-raised`, `--zs-fill-quaternary`
(subtle). Accent: `--zs-accent` / `--zs-accent-hover/active` / `--zs-accent-ink`
(near-white text-on-accent). No dark-theme token — the "contrast" band = accent fill.

---

## ROUND 1 — Foundation (one cohesive change; commit before Round 2)

### 1a. Marketing display type scale (styles.css, above large-title)
```css
--zs-text-display-1-size: 3.5rem;   --zs-text-display-1-line: 3.75rem;  --zs-text-display-1-weight: 700; --zs-text-display-1-tracking: -0.03em;   /* 56 — hero */
--zs-text-display-2-size: 2.75rem;  --zs-text-display-2-line: 3rem;     --zs-text-display-2-weight: 700; --zs-text-display-2-tracking: -0.026em;  /* 44 — stats / big */
--zs-text-display-3-size: 2.125rem; --zs-text-display-3-line: 2.5rem;   --zs-text-display-3-weight: 700; --zs-text-display-3-tracking: -0.022em;  /* 34→ section h2 lift (use 2.125–2.375rem) */
```
(Tune line-heights tight. `text-wrap: balance` on headings. These are rem-only,
token-pure.) Hero h1 → display-1; StatsBand values → display-1/2; section <h2>
titles (FeatureGrid/Cta/StatsBand/Faq + PricingTable header) → a confident
display-3 (~2.25rem) — markedly bigger than today's title-1/title-2.

### 1b. Section tone/surface system (the page-rhythm backbone)
Add a `tone?: "default" | "muted" | "accent"` prop to EVERY section root
(Hero/PricingTable/FeatureGrid/Cta/StatsBand/Faq/Footer). The root stamps
`data-tone={tone}`. One SHARED stylesheet (`sections/_section-tone.css`, imported
once into styles.css under "Page sections") defines full-bleed band treatments:
- `default` — transparent (inherits the page; today's behavior).
- `muted` — a subtle full-bleed surface fill (`--zs-surface` or a soft fill) so the
  band reads as its own panel; gives light/▢ rhythm against default bands.
- `accent` — `background: --zs-accent`; remap inks to accent-ink: the title,
  description, eyebrow, body text inside an `[data-tone="accent"]` section resolve
  to `--zs-accent-ink` / a translucent accent-ink for secondary; links/buttons
  adapt. This is the bold "contrast" band (esp. for the closing CTA).
  forced-colors: accent band falls back to Canvas/CanvasText safely.
- Implementation: the tone classes live in the shared sheet keyed off
  `[data-slot][data-tone="…"]` or a `.zs-section-tone--*` class the root composes —
  pick the cleaner one; document it. Sections keep painting their own structure;
  tone only sets the band background + ink remap. Keep it DRY (one source).

### 1c. Eyebrow treatment (cohesive, stronger)
Today's eyebrows ("WHY ZEROSHIP", "HELP", "READY WHEN YOU ARE") are tiny filler.
Make ONE confident shared eyebrow treatment used across sections: a small
accent-color label, footnote-size, **weight 600, letter-spacing ~0.04em,
uppercase**, with a bit more presence (or a subtle accent-tinted pill). Consistent
across Hero/FeatureGrid/Cta/StatsBand/Faq. (Hero's `<Badge>New</Badge>` ergonomic
path stays valid; this is the section eyebrow style.)

ROUND 1 applies 1a+1b+1c across all 7 sections (typography lift + tone hooks +
eyebrow). Stories: add a `tone` knob where useful and a story showcasing muted +
accent bands (e.g. Cta accent, StatsBand muted). Keep all existing stories green.

---

## ROUND 2 — Targeted per-section polish (after Round 1 committed)

- **Hero**: title display-1; bigger CTA buttons (marketing size — taller/more
  padding via the consumer Buttons in stories + ensure the actions row has presence);
  the **media slot becomes a framed product surface** — the default `Hero.Media`
  placeholder gets a real frame (border + `--zs-shadow-*` elevation + rounded corners
  + a subtle inner content hint), so even the demo reads as a product window, not a
  flat skeleton. Optional subtle accent radial/gradient backdrop behind the hero
  (token-pure, via color-mix) for depth — gated/optional.
- **StatsBand**: values → **display scale (~display-1/2, billboard size)**; tighten
  the band (less dead vertical space), optional thin dividers between stats; label in
  muted caption. Reads as a confident proof bar.
- **PricingTable featured tier**: real **elevation (raise via shadow) + slight
  scale-up** + a **colored top accent bar/cap** (not just the ring); stronger "Most
  popular" badge; make the non-featured CTAs **higher contrast** (the washed tinted
  buttons read as disabled — use outline or a stronger tint).
- **CTA**: support a bold closing band — default to (or add a story for)
  `tone="accent"` with a large display headline + a high-contrast button; tighten so
  it's a punchy closing statement, not a second hero.
- **FeatureGrid**: larger section title; bigger icon badges (more presence); a touch
  more structure per item.
- **FAQ**: refine the **open-state** — the current bright-blue rectangular outline
  reads like a focus/debug ring; soften to a subtle surface tint / left accent / calm
  open treatment (keep the real focus-visible ring distinct from the open state).
- **Footer**: stronger **brand anchoring** (brand wordmark larger/weightier), tighter
  column spacing, and the **social links become Icon buttons** (Lucide via Icon — e.g.
  a generic set; consumer passes glyphs) instead of bare underlined text.

## Constraints (every change)
`--zs-*` only; no raw hex/px (rem/%/dvh/oklch); logical properties; forced-colors +
reduced-motion (incl. the new accent band); no "HIG"/"Apple"; pre-launch
no-back-compat; forwardRef + data-slot preserved; per-prop JSDoc. The tone system +
display tokens must not regress the existing 631/631 suite — keep all stories green;
add stories for the new tone variants. After each round: build + build-storybook +
full-ish suite green + screenshot review.
