# Codex brief — `@zeroship/ui` design system (Storybook + React), chunk 1: foundation + **theming**

## Goal

The builder UI is a prototype. Build a **serious, governed, multi-theme design system** —
the enterprise source of truth — as a reusable React component library documented in
**Storybook**, following `docs/design/ui-design-flow.md`. Branch: `builder/ui-design`
(off main). Package: **`sdks/ui`** (`@zeroship/ui`), consumed by the builder now and by
generated apps later.

**Theming is first-class** (the user wants to try different styles/themes): the token layer
is a **theme contract**, components reference only semantic tokens, and Storybook ships a
**live theme switcher** so any story renders under any theme. This chunk lands the
foundation: tokens + theme contract + **≥3 distinct themes** + core primitives + Storybook
(with theme switcher + a11y) + one migrated builder surface. Full component set + whole-app
re-skin are later chunks.

## Follow the design-flow pattern (`docs/design/ui-design-flow.md`)

### Discover (audit first; write findings into the decision doc)
Codify the EXISTING design language — don't invent generic Material/Chakra. Read:
`apps/zeroship-builder/design/REDESIGN*.md` + the `*.html` mockups;
`apps/zeroship-builder/src/client/index.css` (current Tailwind v4 `@theme` tokens:
paper/ink/tomato/ivy, serif/mono); and the current components (inventory the primitives the
app actually uses).

### Define — the theme contract + token taxonomy
- **Theme contract:** a fixed set of **semantic token keys** every theme MUST provide:
  e.g. `--surface`, `--surface-raised`, `--ink`, `--ink-soft`, `--accent`, `--accent-ink`,
  `--state-{success,warn,danger,info}`, `--rule`, `--focus`, `--radius-{sm,md,lg}`,
  `--font-{display,body,mono}`, `--text-{xs..3xl}`, `--space-*`, `--shadow-*`, `--motion-*`,
  `--z-*`. Components reference ONLY these keys — never raw hex/px. (This is what makes
  themes swappable.)
- **Themes (≥3, genuinely distinct so styles are visibly different):**
  1. **Atelier** — the existing Refined Atelier (editorial; paper/ink; serif display).
  2. **Studio** — clean modern/minimal (neutral grays, sans display, tighter radius,
     denser).
  3. A bold third — e.g. **Dusk** (dark) or a vivid/playful theme — to prove the contract
     across light/dark + personality.
  Each theme = a complete value set for the contract keys.

### Build (chunk 1)
- `sdks/ui` package: `package.json` (`@zeroship/ui`, ESM, `exports`), tsconfig, tsup build
  (match other sdks).
- **Tokens / theming runtime:**
  - The theme contract as CSS custom properties, scoped per theme via `[data-theme="..."]`
    on a root element (and exposed to Tailwind v4 `@theme` so app classes resolve them).
  - A React **`ThemeProvider`** + `useTheme()` that sets `data-theme` (and persists choice);
    switching swaps the whole look instantly with zero component changes.
- **Core primitives** (compose tokens, typed, accessible, theme-correct): `Button`
  (variants/sizes/loading/disabled), `Input`/`Textarea`/`Select`, `Card`, `Badge`/`Chip`,
  `Dialog`/`Modal`, `Tabs`, `Toast`, `Table`, `EmptyState`, `Spinner`. Pick the set the app
  uses (from the audit) — quality over count. Each must render correctly under all themes.
- **Storybook 8** in `sdks/ui`:
  - **Theme switcher** in the toolbar (`@storybook/addon-themes`
    `withThemeByDataAttribute`, or a `globalTypes` toolbar + decorator) listing all themes —
    flip live; every story re-renders under the selected theme. *This is the "try different
    styles" surface.*
  - `@storybook/addon-a11y` enabled, autodocs on.
  - A **Foundations** section: token swatches (color/type/space/radius/shadow) **rendered
    per active theme**, and a **Theming** docs page explaining the contract + how to add a
    theme.
  - A story per primitive covering all states/variants.

### Validate / prove
- **Migrate ONE real builder surface** (suggest `SettingsCanvas` or `LogsCanvas`) to consume
  `@zeroship/ui`, wrapped in `ThemeProvider` (default Atelier) — proves the consume path +
  that the app looks right.
- a11y addon clean on shipped primitives **in every theme** (contrast etc. hold per theme).

## Constraints
- Brand-anchored: Atelier theme = the existing Refined Atelier elevated; the other themes
  are distinct but coherent. Token contract is the ONLY styling source (no raw values in
  components).
- Pre-launch, no back-compat. You have network (Storybook/deps install). ONE builder dev
  server at a time if you run the app (fixed runtime port).
- Do NOT re-skin the whole app this chunk — foundation + theming + one surface.
- **DO NOT commit/merge/push.** The pilot reviews + commits.

## Verify (NO OpenAI needed)
- `pnpm --filter @zeroship/ui build` passes; `pnpm --filter @zeroship/ui build-storybook`
  builds a static Storybook with the theme switcher working.
- **Theme swap works:** switching the Storybook theme toolbar visibly restyles every
  primitive (capture evidence: e.g. the same Button story under Atelier vs Studio vs Dusk),
  and the contract holds (grep components for raw hex/px → none where a token exists).
- `pnpm --filter zeroship-builder build` passes with the migrated surface.
- Relevant builder e2e green for the migrated surface; a11y addon clean per theme.

## Report (stdout)
- Audit findings + design principles + the **theme contract** (token keys) + the 3 themes
  + decision-doc path `docs/decisions/2026-05-26-design-system.md`.
- Files created (`sdks/ui/**`), primitives shipped, the ThemeProvider/switcher mechanism,
  the migrated surface.
- Verification: @zeroship/ui build, static Storybook build, theme-swap evidence, builder
  build, e2e, a11y. What's deferred to chunk 2 (full component set + whole-app re-skin +
  wiring the design-flow §4 gates into Critic/Reviewer).
