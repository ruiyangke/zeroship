# Brief: Glass themes (glass-dark + glass-light) for @zeroship/ui

## Goal
Add two new glassmorphism themes to `@zeroship/ui` — `glass-dark` and `glass-light`
— as themes #4 and #5 alongside `studio` / `atelier` / `dusk`. **Studio stays the
default.** Frosted, translucent panels floating over a colorful blurred backdrop:
the "very modern glass" look (Vision OS / macOS dark / iOS Control Center).

## Hard constraints
- **Pre-launch, no back-compat.** Rename/break freely; no shims.
- **Token contract is the single source of truth.** Components read `--zs-*` tokens
  only; they never hardcode appearance.
- **The other 3 themes MUST be provably untouched.** Do NOT edit the `studio`,
  `atelier`, or `dusk` token blocks. Glass is purely additive (see token strategy).
- **No raw hex, no raw px** in component CSS or `styles.css`. Use `oklch(...)` for
  color and `rem` for lengths — including the blur radius (`blur(1.125rem)`, not px).
- **Keep:** border-box reset, the base-typography rule, the Select SVG glyphs, the
  Checkbox SVG glyph, the 3 existing themes, Base UI primitives.
- **DO NOT commit. DO NOT push. DO NOT merge.** Leave ALL changes uncommitted — the
  pilot reviews, tunes taste, and commits separately.
- Worktree: `.worktrees/ui-design` on branch `builder/ui-design`. You are the only
  writer for the duration of this run.

## Token strategy (this is the crux — get it exactly right)
Glass needs two capabilities the contract lacks. Add them as tokens that DEFAULT to
"off" via `var(…, fallback)`, so studio/atelier/dusk need no edits:

1. `--zs-blur` — backdrop blur amount. Set ONLY in the two glass blocks. Surfaces
   apply `backdrop-filter: var(--zs-blur, none)` so unset = no blur.
2. `--zs-surface-bg` — the backdrop gradient "mesh". Set ONLY in the two glass
   blocks. Roots apply `background: var(--zs-surface-bg, var(--zs-surface))` so
   unset = the flat surface color.

Glass panel surface tokens (`--zs-surface-raised`, `--zs-surface-overlay`) carry
ALPHA (~0.55–0.65) so panels read as frosted sheets. `--zs-rule` carries alpha for
a hairline highlight border.

## Files to change
1. `sdks/ui/src/styles.css`
   - Add a complete `[data-theme="glass-dark"]` block and `[data-theme="glass-light"]`
     block — every token in the contract (copy the full key set from an existing
     block; do not omit any), PLUS `--zs-blur` and `--zs-surface-bg`.
   - Change `.zs-theme-root { background: var(--zs-surface); }` →
     `background: var(--zs-surface-bg, var(--zs-surface));`
   - Do NOT touch the studio/atelier/dusk blocks.
2. `sdks/ui/src/stories/story.css`
   - `.zs-story-main` background → `var(--zs-surface-bg, var(--zs-surface))` (and the
     `.zs-story-main--docs` if it sets a background) so demos show the mesh.
3. Surface-bearing component CSS — add these two lines to the top-level panel rule of
   EACH (the floating/container surface, not inner controls):
   ```
   backdrop-filter: var(--zs-blur, none);
   -webkit-backdrop-filter: var(--zs-blur, none);
   ```
   Components: `Card` (.zs-card), `Dialog` (popup), `Popover` (popup),
   `Select` (.zs-select__popup), `Menu` (popup), `Toast`, `Tooltip`.
   Do NOT add backdrop-filter to inner form controls (Input/Select trigger/Textarea/
   Checkbox/Radio/Switch) — they sit inside frosted cards; double-blur muddies + costs
   perf. They get frost only via the translucent `--zs-surface-raised` token value.
4. `sdks/ui/src/theme.tsx` — add `"glass-dark"`, `"glass-light"` to the `themes`
   array and to `themeLabels` ("Glass Dark", "Glass Light"). Keep `DEFAULT_THEME =
   "studio"`.
5. `sdks/ui/.storybook/preview.ts` — add both to the `withThemeByDataAttribute`
   `themes` map (`"Glass Dark": "glass-dark"`, `"Glass Light": "glass-light"`).
   Keep defaultTheme Studio.
6. `sdks/ui/scripts/check-storybook-a11y.mjs` — add the two themes to its theme list
   (it currently iterates 3; make it 5).
7. `sdks/ui/src/stories/Theming.mdx` — document the two glass themes and the two new
   contract tokens (`--zs-blur`, `--zs-surface-bg`), noting they default off.

## Starting token values (TUNE against screenshots + a11y; these are a strong start)
glass-dark:
- `--zs-surface-bg`: layered radial mesh on near-black, MUTED stops (not neon), e.g.
  `radial-gradient(at 18% 18%, oklch(0.45 0.13 285 / 0.55), transparent 52%),
   radial-gradient(at 82% 22%, oklch(0.5 0.1 215 / 0.45), transparent 55%),
   radial-gradient(at 65% 85%, oklch(0.45 0.12 330 / 0.4), transparent 50%),
   oklch(0.16 0.025 280)`
- `--zs-surface`: `oklch(0.16 0.025 280)` (solid fallback / flat uses)
- `--zs-surface-raised`: `oklch(0.32 0.03 280 / 0.55)`
- `--zs-surface-overlay`: `oklch(0.34 0.035 280 / 0.62)`
- `--zs-surface-sunken`: `oklch(0.22 0.025 280 / 0.5)`
- `--zs-ink`: `oklch(0.96 0.01 280)`; `--zs-ink-soft`: `oklch(0.82 0.02 280)`;
  `--zs-ink-muted`: `oklch(0.68 0.02 280)`
- `--zs-accent`: `oklch(0.8 0.13 205)` (luminous cyan); `--zs-accent-hover`:
  `oklch(0.86 0.12 205)`; `--zs-accent-ink`: `oklch(0.17 0.03 280)`
- `--zs-rule`: `oklch(1 0 0 / 0.14)`; `--zs-rule-strong`: `oklch(1 0 0 / 0.28)`
- `--zs-focus`: `oklch(0.8 0.13 205)`; `--zs-backdrop`: `oklch(0.05 0.02 280 / 0.6)`
- `--zs-blur`: `blur(1.125rem) saturate(1.4)`
- state colors: luminous-on-dark (mirror dusk's approach)
- shadows: soft + large with a faint accent glow; reuse the dusk shadow scale magnitudes
- fonts/space/radius/motion/z/border/text: copy studio's (Inter, same scale)

glass-light:
- `--zs-surface-bg`: pastel mesh on near-white, e.g.
  `radial-gradient(at 15% 15%, oklch(0.88 0.07 255 / 0.7), transparent 55%),
   radial-gradient(at 85% 20%, oklch(0.9 0.06 195 / 0.6), transparent 55%),
   radial-gradient(at 78% 85%, oklch(0.89 0.07 320 / 0.55), transparent 55%),
   oklch(0.97 0.012 255)`
- `--zs-surface`: `oklch(0.97 0.012 255)`
- `--zs-surface-raised`: `oklch(1 0 0 / 0.55)`; `--zs-surface-overlay`: `oklch(1 0 0 / 0.66)`;
  `--zs-surface-sunken`: `oklch(0.95 0.01 255 / 0.5)`
- `--zs-ink`: `oklch(0.25 0.02 275)`; `--zs-ink-soft`: `oklch(0.42 0.02 275)`;
  `--zs-ink-muted`: `oklch(0.55 0.02 275)`
- `--zs-accent`: `oklch(0.55 0.18 285)` (saturated indigo); `--zs-accent-hover`:
  `oklch(0.49 0.19 285)`; `--zs-accent-ink`: `oklch(0.99 0.005 255)`
- `--zs-rule`: `oklch(1 0 0 / 0.6)` over the frost; `--zs-rule-strong`:
  `oklch(0.55 0.02 275 / 0.4)`
- `--zs-focus`: `oklch(0.55 0.18 285)`; `--zs-backdrop`: `oklch(0.5 0.05 275 / 0.3)`
- `--zs-blur`: `blur(1rem) saturate(1.25)`
- state colors: mirror studio's (light-bg variants)
- shadows: soft, lower-alpha; reuse studio's shadow scale magnitudes

## Verify before you report (you have network via bypass; localhost OK)
1. `pnpm --filter @zeroship/ui build` — green (ESM + DTS).
2. `pnpm --filter @zeroship/ui build-storybook` — green.
3. Token purity: `grep -rnE '#[0-9a-fA-F]{3,8}' src --include='*.css'` and
   `grep -rnoE '[0-9]+px' src --include='*.css'` → BOTH empty.
4. a11y across all 5 themes: serve `storybook-static` and run
   `STORYBOOK_URL=http://localhost:<port> node scripts/check-storybook-a11y.mjs`.
   It MUST report clean (no serious/critical) for all 12 stories × 5 themes. If
   glass-dark/glass-light fail CONTRAST, desaturate/darken the mesh stops and/or
   raise panel opacity until they pass — DO NOT lower the bar. Re-run until clean.
5. Capture evidence screenshots (playwright/chromium, deviceScaleFactor 2) into
   `sdks/ui/storybook-static/theme-evidence/` for BOTH glass themes, stories:
   `primitives-forms--input-textarea-select` (or the Project-settings form story),
   `components-base-ui--choice-controls`, `components-base-ui--portaled-popup-proof`
   (dialog + open select), `components-base-ui--button-states`. Filenames like
   `glass-dark-<story>.png`. Use `globals=theme:Glass%20Dark` / `Glass%20Light`.

## Report (stdout)
Print: files changed, the final `--zs-blur`/mesh values you settled on, the a11y
result line, the token-purity result, and the list of evidence screenshot paths.
State clearly that you did NOT commit. Note anything you were unsure about for the
pilot to judge on taste.
