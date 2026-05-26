# Codex brief — rewrite `@zeroship/ui` on **Base UI** (headless primitives)

## Goal

Rewrite the `@zeroship/ui` component layer (`sdks/ui`) on top of **Base UI**
(`@base-ui/react`, v1.5.0 — the unstyled/accessible primitives from the
Radix/Floating-UI/MUI teams). **Keep everything that makes the system ours**; replace
only the hand-rolled component internals with Base UI for battle-tested accessibility
(focus traps, keyboard nav, ARIA, popup positioning, enter/exit animation).

Branch/worktree: you are run with `-C .worktrees/ui-design` (branch `builder/ui-design`).
**DO NOT commit / merge / push** — the pilot reviews and commits.

### KEEP (do not regress)
- The **token contract** (`src/tokens.ts`, `src/styles.css`, `src/tailwind.css`) and the
  `--zs-*` semantic CSS variables. Components reference ONLY tokens — never raw hex/px.
- **`ThemeProvider` + `useTheme` + 3 themes** (atelier / studio / dusk) and runtime
  `data-theme` swapping. Atelier defaults live on `:root` (already fixed — keep it).
- **Storybook 8** setup: `addon-themes` toolbar theme-switcher, `addon-a11y`, autodocs,
  Foundations + Theming docs pages, the `scripts/capture-theme-evidence.mjs` and
  `scripts/check-storybook-a11y.mjs`.
- The **Refined Atelier** visual language (editorial; paper/ink; serif display; tomato
  accent). The other two themes stay distinct.
- The **ergonomic public API** (see "API stability" below). The migrated
  `apps/zeroship-builder` SettingsCanvas must keep compiling/working unchanged.

### REPLACE (hand-rolled → Base UI)
Rebuild the interactive primitives on Base UI parts, styled with our tokens:
**Dialog, Popover, Tooltip, Select, Menu (dropdown), Tabs, Switch, Checkbox, Radio/
RadioGroup, Accordion, Toast, Separator**, and use **Base UI `Field` / `Fieldset` / `Form`**
to wire label/description/error/validation a11y for `Input` / `Textarea` / `Select`
(replacing the hand-rolled `FieldChrome`). Consider Base UI `Slider`, `NumberField`,
`Progress`, `ScrollArea`, `Collapsible`, `Toolbar`, `Avatar`, `Combobox/Autocomplete`
where the builder app needs them (audit `apps/zeroship-builder/src/client/`).

Primitives with no behavioral need (`Card`, `Badge`/`Chip`, `Spinner`, `Table`,
`EmptyState`) stay bespoke — but still move into the new per-component structure.

## Structure — split `primitives.tsx` → per-component dirs

Replace the single `src/primitives.tsx` with `src/components/<Name>/`:
```
src/components/Button/{Button.tsx, Button.css, index.ts}
src/components/Dialog/{Dialog.tsx, Dialog.css, index.ts}
... one dir per component ...
```
- Co-locate each component's CSS in its own `<Name>.css` (token-only). Keep the global
  token layer in `src/styles.css` (themes + `:root`). The package CSS entry
  (`styles.css` exported as `@zeroship/ui/styles.css`) must `@import` all component CSS
  so a single import still styles everything (verify `copy-css.mjs` still works, adjust
  if needed).
- `src/index.ts` barrel re-exports every component + `ThemeProvider`/`useTheme`/tokens —
  keep the existing export names stable.

## API stability (IMPORTANT — AI-friendly surface)

Creators/the Builder consume ergonomic single components, NOT Base UI's raw compound
parts. Preserve the current ergonomic wrapper API on the default exports:
- `<Dialog open onOpenChange title? footer?>{children}</Dialog>` — internally Base UI
  `Dialog.Root/Trigger?/Portal/Backdrop/Popup` + `Dialog.Close` inside the popup.
- `<Select label? hint? error? value onValueChange>{<option>…}</Select>` OR an `items`
  prop — internally Base UI `Select.Root/Trigger/Value/Icon/Portal/Positioner/Popup/Item`.
  Keep the SAME props SettingsCanvas already passes.
- `<Tabs items value onValueChange>` — internally Base UI `Tabs.Root/List/Tab/Panel`.
- `<Input/Textarea label hint error>` — internally Base UI `Field.Root/Label/Control/
  Description/Error`.
It is fine to ALSO export the lower-level compound parts for advanced composition, but the
ergonomic wrappers are the primary surface and must not break existing callers. Follow
`docs/reference/api-design-guidelines.md`.

## Theming gotcha — portaled popups MUST be themed (verify!)

Base UI portals `Popup`/`Positioner` content to `document.body` by default — OUTSIDE the
app's `ThemeProvider` wrapper div. If `data-theme` is set on an inner div, portaled
Dialog/Select/Menu/Popover/Tooltip popups will LOSE the theme tokens and render unstyled.

Fix: `ThemeProvider` must set `data-theme` on **`document.documentElement`** (the `<html>`
element) — not on an inner wrapper — so portaled popups inherit the theme tokens. (Atelier
is on `:root`, studio/dusk on `[data-theme=...]`, so `<html>` is the correct host.)
Storybook already sets it on `html` via `addon-themes parentSelector:"html"` — keep that.

**Prove it:** screenshot a Dialog (or Select) OPEN popup under the **dusk** theme and
confirm it is themed (dark surface, coral accent), not white/unstyled. Save evidence
alongside the existing theme-evidence.

## Styling Base UI parts

- Style every part via `className` with our token classes (e.g. `.zs-dialog__popup`,
  `.zs-select__item`). Plain global CSS classes work (the className accepts a string or a
  state→string fn).
- Use Base UI's `data-*` state attributes for states:
  `data-popup-open`, `data-highlighted`, `data-selected`, `data-disabled`,
  `data-checked`, `data-placeholder`, and the animation hooks
  `data-starting-style` / `data-ending-style` (+ the `--transform-origin` CSS var) for
  enter/exit transitions. Drive transitions off our `--zs-motion-*` tokens.
- Keep focus rings on our `--zs-focus` / `--zs-shadow-focus`.

## Install

1. Verify + install the package (network — you are run with the bypass flag):
   `pnpm --filter @zeroship/ui add @base-ui/react@^1.5.0`
   (Confirm with `npm view @base-ui/react version` first; if the name ever fails, the
   legacy name is `@base-ui-components/react`, but `@base-ui/react` 1.5.0 is correct.)
2. React 18/19 peer — match the repo's React version.

## Verify (NO OpenAI needed — do all of this and report results)
- `pnpm --filter @zeroship/ui build` → ESM + DTS, green.
- `pnpm --filter @zeroship/ui build-storybook` → static Storybook builds; theme switcher
  present. (Use the STATIC build for verification — do NOT start the dev server on :6006.)
- A story per component, covering all states/variants, each rendering under all 3 themes;
  `addon-a11y` clean (no serious/critical) per theme.
- **Portaled-popup theming proof** (dusk Dialog/Select open) — screenshot saved.
- Keyboard a11y sanity: Dialog traps focus + Escape closes; Select/Menu arrow-key
  navigation + type-ahead; Tabs arrow-key roving. (Base UI provides these — confirm in a
  story interaction or note verified.)
- `grep -rnE "#[0-9a-fA-F]{3,8}|[0-9]+px" src/components src/styles.css` → no raw values
  where a token exists.
- `pnpm --filter zeroship-builder build` → green with the migrated SettingsCanvas (and any
  surfaces you re-skin). Run the relevant builder e2e for the migrated surface.

## Decision doc
Append a section to `docs/decisions/2026-05-26-design-system.md`: "Adopt Base UI for
interactive primitives" — what/why (a11y maturity without giving up the token contract,
package model, or Tailwind-optional theming; rejected shadcn because copy-in + Tailwind
fights the platform-inheritance goal), the keep/replace split, and the portal-theming
requirement.

## Report (stdout)
- Package + version installed; per-component dir structure; which primitives are Base-UI
  backed vs bespoke; the ergonomic-wrapper APIs preserved.
- All verification results above (builds, storybook, a11y per theme, portal-theming proof,
  grep, builder build + e2e).
- Anything deferred (e.g. surfaces not yet re-skinned, components not yet ported).

## Constraints
- Pre-launch, no back-compat shims. Rename/replace freely; just keep the documented public
  export names + ergonomic wrapper props so existing callers compile.
- Token contract is the ONLY styling source. No raw hex/px in components.
- ONE builder dev server / no :6006 dev server during verify (use static build).
- **DO NOT commit / merge / push.**
