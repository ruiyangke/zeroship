# 2026-05-26 — Glass themes (glass-dark + glass-light)

## Decision
Add two glassmorphism themes to `@zeroship/ui` — `glass-dark` and `glass-light` —
as the 4th and 5th themes alongside `studio` / `atelier` / `dusk`. **Studio remains
the default.**

## Why two tokens, not a fork
Glass is the one look that a pure color-token swap cannot express: it needs a blurred
backdrop behind translucent panels. Rather than special-case glass in component code,
the theme contract gains two tokens that **default to "off"** so the other three themes
are provably untouched:

- `--zs-blur` — backdrop blur amount. Surfaces apply `backdrop-filter: var(--zs-blur, none)`,
  so any theme that doesn't set it gets no blur and no cost.
- `--zs-surface-bg` — the backdrop gradient "mesh". Roots apply
  `background: var(--zs-surface-bg, var(--zs-surface))`, so unset = the existing flat color.

Glass panels also carry **alpha** in `--zs-surface-raised` / `--zs-surface-overlay`
(~0.55–0.65) so they read as frosted sheets, and `--zs-rule` carries alpha for a hairline
highlight border. No component reads anything new beyond the two tokens.

The diff touches only: the two new theme blocks, one `.zs-theme-root` background line, the
seven surface components that float (`Card`, `Dialog`, `Popover`, `Select`, `Menu`, `Toast`,
`Tooltip`), the demo background in `story.css`, theme registration (`theme.tsx`,
`preview.ts`, the a11y script), and `Theming.mdx`. The studio/atelier/dusk token blocks are
unchanged.

## Which surfaces blur
Only top-level floating/container surfaces get `backdrop-filter`. Inner form controls
(Input, Select trigger, Textarea, Checkbox, Radio, Switch) do **not** — they sit inside a
frosted card, so a second backdrop-filter would muddy the result and cost perf. They get
frost only via the translucent `--zs-surface-raised` value.

## Contrast
Translucent panels over a gradient are the classic a11y risk. Mitigated by keeping the mesh
stops muted (not neon), panel opacity high enough to read near-solid after blur, and strong
ink. Verified: a11y clean across all 12 stories × 5 themes (no serious/critical), token
purity clean (no raw hex / no raw px — blur radius in `rem`).

## Alternatives rejected
- **Make glass the default** — glass-by-default is too strong a taste commitment for every
  generated app; Studio stays default, glass is opt-in.
- **Special-case glass in component JS/CSS** — would scatter theme knowledge into components
  and break the "components read tokens only" invariant.
