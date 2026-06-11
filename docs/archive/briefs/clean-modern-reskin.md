# Codex brief — clean-modern re-skin of @zeroship/ui (learn from Base UI's styled examples)

## Goal
The current look (serif + heavy tomato + sharp corners + ALL-CAPS labels) reads dated/ugly.
Re-skin `@zeroship/ui` to a **clean, modern aesthetic modeled on Base UI's official styled
examples**, and make it the DEFAULT. Keep the token-contract + ThemeProvider + 3-theme
architecture + Base UI components + the recent fixes (border-box reset, Select SVG glyphs).
This is execution-quality-critical: the user rejected the current styling. Make it genuinely
polished.

Worktree: `-C .worktrees/ui-design` (branch `builder/ui-design`). **DO NOT commit / merge /
push** — the pilot reviews, screenshots, and commits.

## STEP 1 — Study Base UI's styled examples (you have network)
Fetch and study the official reference styling, and record concrete values in the decision
doc:
- `https://base-ui.com/react/components/select`, `/menu`, `/dialog`, `/popover`,
  `/checkbox`, `/switch`, `/tabs`, `/input` (or `/field`), and the handbook
  `/react/handbook/styling` + `/react/handbook/animation`.
- Their GitHub example CSS (e.g. the `*.module.css` files under
  `github.com/mui/base-ui/tree/master/docs` component demos).
Extract: border-radius scale, box-shadow (popups + controls), focus ring/outline treatment,
control heights + padding, type sizes/weights, **label style (they use sentence-case, not
all-caps)**, hover/highlighted/selected item treatment, and popup enter/exit motion
(transform-origin + scale/opacity, ~150–250ms).

## STEP 2 — Global fixes (apply to ALL themes; these are the biggest "dated" tells)
- **Labels** (`.zs-field__label` in styles.css / Field.css): remove `text-transform:
  uppercase` and the heavy `letter-spacing`; use sentence-case, medium weight (~550–600),
  `--zs-text-sm`, color `--zs-ink-soft`. (Keep the `--zs-letter-label` token only if still
  used elsewhere.)
- **Field alignment**: make the label→control gap identical across Input / Textarea /
  Select / Checkbox / Switch / RadioGroup. Right now a Select in a 2-col row sits lower than
  an Input (inconsistent FieldFrame spacing) — fix so paired fields top-align.
- **Focus ring**: soft Base-UI-style ring (e.g. 2–3px ring in accent at low alpha, optional
  1px offset), consistent across controls.
- **Popup motion**: Select/Menu/Popover/Tooltip/Dialog use scale+opacity enter/exit via
  `data-starting-style`/`data-ending-style` (+ `--transform-origin`), Base-UI timing.
- Keep the border-box reset and the Select `<svg>` chevron/check (do NOT reintroduce the
  emoji-default glyphs).

## STEP 3 — Retune tokens toward clean-modern
- **Default theme = clean modern** (promote/retune the existing "studio" direction to the
  Base UI polish and put it on `:root` as the default). Neutral palette, sans display +
  body (drop the serif as the DEFAULT display), restrained accent used sparingly, soft
  radii, soft layered low-alpha shadows, comfortable spacing.
- Keep **Atelier** (editorial serif/tomato) and **Dusk** (dark) as switchable variants —
  but adopt the same softer radii/shadow scale + sentence-case labels so all three look
  modern; the variants differ by palette/personality, not by dated execution.
- Radii: soften (sharp ~2px → controls ~`0.5rem`, popups ~`0.625–0.75rem`, chips/pill as
  appropriate). Shadows: soft, layered, low-opacity (hairline border + a soft drop).
- Components reference ONLY `--zs-*` tokens — no raw hex/px (keep the contract).

## STEP 4 — Apply across the whole component set
Re-skin every component (Button, Card, Badge/Chip, Input/Textarea/Select, Checkbox/Radio/
Switch, Dialog/Popover/Tooltip/Menu, Tabs/Accordion, Table, Toast, EmptyState, Spinner,
Separator) so they're cohesive + polished under the default and all 3 themes. Also sanity-
check the **Switch** thumb (currently looks stuck left on a colored track — fix the
checked/unchecked thumb translate + track color).

## Verify (NO OpenAI)
- `pnpm --filter @zeroship/ui build` (ESM+DTS) + `build-storybook` (static) green.
- a11y clean: `STORYBOOK_URL=… node scripts/check-storybook-a11y.mjs` → 12 stories × 3
  themes, no serious/critical (contrast must hold for the new palette).
- `grep -rnE "#[0-9a-fA-F]{3,8}|[0-9]+px" src/components src/styles.css` → none where a token
  exists.
- `pnpm --filter zeroship-builder build` green (SettingsCanvas still compiles).
- **Capture evidence screenshots** (default theme + each theme) for: the Project-settings
  form, an open Select, a Dialog, Button states, the choice controls. Save under
  `storybook-static/theme-evidence/` so the pilot can review the new look before committing.

## Report (stdout)
- The Base UI values you adopted (radii/shadow/focus/type/motion) + decision doc path
  (`docs/decisions/2026-05-26-clean-modern-reskin.md`); the global label/alignment/focus/
  motion changes; the new default theme + retuned variants; per-component notes; all
  verification output + the screenshot paths.

## Constraints
- Network available (study Base UI) — run with the bypass flag. NO OpenAI.
- Token contract is the only styling source; keep the 3-theme architecture + ThemeProvider
  (data-theme on documentElement) + Base UI components + border-box + SVG glyphs.
- **DO NOT commit / merge / push.**
