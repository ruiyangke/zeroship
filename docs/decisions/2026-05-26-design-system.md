# 2026-05-26: Governed multi-theme design system

## Status

Accepted for chunk 1.

## Context

Zeroship-builder is pre-launch, so this change establishes the design-system
shape directly instead of preserving prototype APIs. The builder UI already has
an Atelier redesign direction in `apps/zeroship-builder/design/REDESIGN.md`,
`REDESIGN_FULL.md`, and the HTML mockups. This decision turns that direction
into a reusable React package at `sdks/ui` (`@zeroship/ui`) with a fixed
semantic theme contract, Storybook documentation, and one real builder surface
using the package.

## Audit

Sources read:

- `docs/archive/briefs/design-system.md`
- `docs/design/ui-design-flow.md`
- `apps/zeroship-builder/design/REDESIGN.md`
- `apps/zeroship-builder/design/REDESIGN_FULL.md`
- `apps/zeroship-builder/design/redesign-preview.html`
- `apps/zeroship-builder/design/workspace-preview.html`
- `apps/zeroship-builder/design/complete-redesign.html`
- `apps/zeroship-builder/src/client/index.css`
- `apps/zeroship-builder/src/client/components/*`
- `apps/zeroship-builder/src/client/workspace/canvases/*`

Findings:

- The brand is not generic SaaS chrome. The current direction is "Atelier":
  paper, ink, editorial typography, hairline rules, sparse tomato action color,
  and calm ledger/form surfaces.
- The existing Tailwind v4 theme already encodes the first palette:
  paper/paper-2/paper-3, ink/ink-soft/pencil, rule/rule-2, tomato, ivy,
  cobalt, amber, and blood.
- The mockups add the product principles: creator workshop, serif display,
  measured motion, asymmetric editorial spacing, plain-language logs, and
  developer surfaces demoted behind calm drawers.
- The builder currently has local primitives (`Button`, `StampButton`,
  `GhostButton`, `Modal`, `Toast`, `EmptyState`, `Pill`, `FilterPill`,
  `Spinner`, cards and form fields) repeated as app code. Chunk 1 moves the
  reusable foundation into `@zeroship/ui` without re-skinning the whole app.
- `SettingsCanvas` is the best first consumption proof: it exercises form
  fields, plan cards, status badges, async actions, destructive actions, and a
  confirmation dialog.

## Principles

1. Theme is a contract, not a color map. Every theme must provide the same
   semantic custom properties.
2. Components read semantic `--zs-*` variables only. Theme values can contain
   raw values; component rules cannot reach for brand colors directly.
3. Atelier remains the default brand. Other themes must be genuinely distinct
   while preserving the same component semantics.
4. Storybook is the governance surface: token swatches, autodocs, states,
   a11y, and a toolbar theme switcher are part of the system, not garnish.
5. App migration is incremental. Chunk 1 proves consumption on one real surface;
   whole-app re-skinning is deferred.

## Theme Contract

Every theme must define these keys:

Color:

`--zs-surface`, `--zs-surface-raised`, `--zs-surface-sunken`,
`--zs-surface-overlay`, `--zs-ink`, `--zs-ink-soft`, `--zs-ink-muted`,
`--zs-accent`, `--zs-accent-hover`, `--zs-accent-ink`,
`--zs-state-success`, `--zs-state-success-bg`, `--zs-state-warn`,
`--zs-state-warn-bg`, `--zs-state-danger`, `--zs-state-danger-bg`,
`--zs-state-info`, `--zs-state-info-bg`, `--zs-rule`,
`--zs-rule-strong`, `--zs-focus`, `--zs-backdrop`, `--zs-code-bg`,
`--zs-code-ink`.

Type:

`--zs-font-display`, `--zs-font-body`, `--zs-font-mono`,
`--zs-text-xs`, `--zs-text-sm`, `--zs-text-md`, `--zs-text-lg`,
`--zs-text-xl`, `--zs-text-2xl`, `--zs-text-3xl`,
`--zs-line-tight`, `--zs-line-normal`, `--zs-line-relaxed`,
`--zs-letter-label`.

Space:

`--zs-space-0`, `--zs-space-1`, `--zs-space-2`, `--zs-space-3`,
`--zs-space-4`, `--zs-space-5`, `--zs-space-6`, `--zs-space-8`,
`--zs-space-10`, `--zs-space-12`, `--zs-space-16`.

Shape, elevation, motion, layers:

`--zs-radius-xs`, `--zs-radius-sm`, `--zs-radius-md`,
`--zs-radius-lg`, `--zs-radius-xl`, `--zs-radius-pill`,
`--zs-shadow-xs`, `--zs-shadow-sm`, `--zs-shadow-md`,
`--zs-shadow-lg`, `--zs-shadow-focus`, `--zs-motion-fast`,
`--zs-motion-base`, `--zs-motion-slow`, `--zs-motion-ease`,
`--zs-z-dropdown`, `--zs-z-popover`, `--zs-z-modal`, `--zs-z-toast`,
`--zs-border-sm`, `--zs-border-md`.

The package also exports `@zeroship/ui/tailwind.css`, mapping Tailwind v4
`@theme` names like `--color-zs-surface`, `--font-zs-display`,
`--text-zs-md`, `--radius-zs-md`, and `--shadow-zs-md` to the semantic
contract. App bundles import `@zeroship/ui/styles.css`; Tailwind-aware
scaffolds may additionally import the Tailwind contract file.

## Themes

### Atelier

Default. Elevated version of the existing Refined Atelier language:

- Paper and ink surfaces (`oklch(0.97 0.012 89)` base).
- Fraunces display, Inter/Public Sans-style operational body, JetBrains Mono
  for code and URLs.
- Tomato accent reserved for primary actions and destructive emphasis, tuned
  slightly darker than the prototype value where foreground contrast requires it.
- Ivy, amber, cobalt, and blood keep current semantic state roles.
- Tight hairlines, low radius, editorial form and ledger feel.

### Studio

Clean modern/minimal alternative:

- Near-white neutral surfaces and cool gray rules.
- Sans display and body for a more product-operations feel.
- Blue accent and denser spacing/radius values.
- Same success/warn/danger/info roles, tuned for light UI contrast.

### Dusk

Dark/bold proof theme:

- Deep charcoal surfaces with raised panels and warm light ink.
- Coral action accent, cyan info, lime success, amber warning.
- Larger radius and stronger shadows than Atelier/Studio.
- Same component contract across a dark environment.

## Decision

Create `sdks/ui` as the design-system package:

- ESM package built by tsup, matching the other SDK packages.
- `ThemeProvider` plus `useTheme()` writes `data-theme` and persists user
  choice by default.
- Theme values live in CSS custom properties scoped by `[data-theme]`.
- Core primitives for chunk 1: Button, Input, Textarea, Select, Card,
  Badge, Chip, Dialog, Tabs, Toast, Table, EmptyState, Spinner.
- Storybook 8 with `@storybook/addon-themes`, `@storybook/addon-a11y`,
  autodocs, a Foundations token-swatch story, and Theming docs.
- Builder wraps the app in `ThemeProvider defaultTheme="atelier"` and migrates
  `SettingsCanvas` to consume `@zeroship/ui` primitives.

## Deferred

- Full component catalog and page templates.
- Whole-builder re-skin and removal of local duplicate primitives.
- Product-level theme picker in the builder UI.
- Wiring `docs/design/ui-design-flow.md` section 4 gates into Critic and
  Reviewer blocker kinds.

## Adopt Base UI for interactive primitives

The component layer now builds its interactive primitives on
`@base-ui/react` while keeping the Zeroship token contract, package shape, and
Tailwind-optional theming model. Base UI gives us maintained focus management,
ARIA wiring, keyboard navigation, popup positioning, and animation lifecycle
attributes without forcing a visual system. The visual layer remains ours:
components still read only semantic `--zs-*` tokens, and the Refined Atelier,
Studio, and Dusk themes continue to provide the complete contract.

The keep/replace split is deliberate. Dialog, Popover, Tooltip, Select, Menu,
Tabs, Switch, Checkbox, Radio/RadioGroup, Accordion, Toast, Separator, and form
field a11y are Base-UI-backed wrappers. Button, Card, Badge, Chip, Spinner,
Table, and EmptyState remain bespoke because they do not need headless
behavior. The public API stays ergonomic for creators and builder code: wrappers
like `Dialog`, `Select`, `Tabs`, `Input`, and `Textarea` preserve their
single-component props while exposing compound parts only for advanced use.

We rejected shadcn-style copy-in primitives for this layer. Copying component
source would couple the system to Tailwind conventions and make portal
inheritance harder to reason about. Base UI lets the package own styling and
tokens directly while relying on a shared accessibility implementation.

Because Base UI portals popups outside the app wrapper, `ThemeProvider` must
write `data-theme` to `document.documentElement`. The `<html>` element is the
theme host for runtime apps and Storybook, so Dialog, Select, Menu, Popover,
Tooltip, and Toast content inherit the correct theme tokens even when rendered
under `document.body`.
