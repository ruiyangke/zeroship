# 2026-05-29 — @zeroship/ui Layouts + Blocks

**Status:** design, approved for spec (forks locked 2026-05-29).
**Scope owner:** `@zeroship/ui` (`sdks/ui`).
**Drives:** the `component-slice-flow` skill, one slice per piece.

---

## 1 · Context

`@zeroship/ui` today is a mature *primitive* layer: ~38 Base-UI-backed
components, each with a story, a `play()` interaction/a11y gate, and a
per-component CSS file reading only semantic `--zs-*` tokens. The Storybook
Test Runner (smoke render + axe on every story) is the single quality gate.

What it lacks is a **composition layer**: there are zero layout primitives
(no `Stack`/`Grid`/`Container`/`AppShell`) and zero composed *blocks*
(no `EmptyState`, `StatCard`, `DataTable`). The original design decision
(`docs/decisions/2026-05-26-design-system.md`) explicitly **deferred** "full
component catalog and page templates" — this spec is that deferred chunk.

The first consumer is **the builder's own UI** (`apps/zeroship-builder`),
which currently carries ~23 local app-code components
(`EmptyState`, `ErrorState`, `Kpi`, `Pill`, `FilterPill`, `LedgerRow`,
`Receipt`, `LiveBanner`, `Skeleton`, `Spinner`, `PageFrame`, `TopBar`,
`WorkspaceShell`, `ProjectCard`, `TemplateCard`, …). Most are un-governed
duplicates of patterns that belong in the design system. Those duplicates are
the demand signal that drives the catalog below.

### Token reality (important)

The decision doc describes an "Atelier" paper/ink theme
(`--zs-surface`/`--zs-ink`). **The shipped code is a different system** — an
Apple-HIG-style "crystal" theme (`themes = ["crystal"]`). Layouts and blocks
**MUST** compose against the *live* token families, not the decision doc:

- spacing: `--zs-space-0 … --zs-space-10`, `--zs-space-half`
- radius: `--zs-radius-1 … --zs-radius-7`, `--zs-radius-full`
- surface: `--zs-surface`, `--zs-surface-bg`, `--zs-surface-raised`,
  `--zs-surface-sunken`, `--zs-surface-overlay`
- fill: `--zs-fill`, `--zs-fill-secondary/tertiary/quaternary`
- label: `--zs-label`, `--zs-label-secondary/tertiary/quaternary`
- separators: `--zs-separator`, `--zs-separator-strong`
- material: `--zs-material-{ultra-thin,thin,regular,thick,ultra-thick,tint}`
- shadow: `--zs-shadow-1 … --zs-shadow-4`, `--zs-shadow-dialog/popover`
- type scale: `--zs-text-*` (large-title … caption-2)

Reconciling the stale decision doc is out of scope here (noted in §8).

---

## 2 · Goals / Non-goals

**Goals**
- Add a **layered layout model** (A3): generic layout primitives + 2 app-shell
  compositions built from them.
- Add the **composed blocks** the builder's local duplicates prove demand for.
- Hold every new piece to the existing contract and quality gate — no new
  conventions, no new gate.

**Non-goals (this round)**
- **No builder migration.** We land governed pieces; deleting the builder's
  local duplicates and rewiring its surfaces is a separate follow-up spec.
- No full page *templates* (whole `SettingsPage`). Blocks + layouts only.
- No new theme; no reconciliation of the Atelier-vs-crystal doc drift.
- No raw SQL / data-fetching inside blocks — `DataTable` is presentational;
  the consumer owns data.

---

## 3 · Architecture & conventions (inherited, non-negotiable)

1. **Same package, new subtrees.** `src/layouts/<Name>/` and
   `src/blocks/<Name>/`, each `{Name}.tsx` + `{Name}.css` + `index.ts`,
   re-exported through `src/index.ts`. Per-component CSS `@import`ed into
   `src/styles.css`.
2. **Same component contract as `Card`:** `forwardRef`; `asChild` via the
   local `Slot`; compound subparts expose `data-slot="<name>"`; dev-warn on
   invalid render-as targets; JSDoc header explaining decisions/anti-patterns.
3. **Semantic tokens only.** Component CSS reads `--zs-*` exclusively — no raw
   hex, no raw px (rem/oklch only, matching the styles.css hard rules). Layout
   gaps/paddings map to the `--zs-space-*` scale via a closed `Gap`/`Pad` union,
   never arbitrary values.
4. **Layout primitives are unstyled-by-default surfaces.** `Stack`/`Grid`/
   `Cluster`/`Center` paint nothing (no bg/border) — they only arrange. Only
   `Container`, `AppShell`, `PageHeader` carry surface treatment.
5. **Same quality gate.** Every piece ships a `.stories.tsx` with: all variants
   as smoke stories, a `play()` where behavior beats markup (per
   `.storybook/CONVENTIONS.md`), and axe-clean by default. A skipped axe rule
   needs a comment justifying it.
6. **Commit-only.** No `git push` (durable user rule). One commit per landed
   slice, following the existing `fix(ui/<name>): …` / `feat(ui/<name>): …`
   message style.

---

## 4 · Catalog

Closed shared unions (defined once, reused): `Gap`/`Pad` = `0 | "half" | 1 …
10` (→ `--zs-space-*`); `Align` = `start | center | end | stretch`; `Justify`
= `start | center | end | between | around | evenly`.

### 4a · Layout primitives

| Piece | Purpose / generalizes | API sketch | Notes |
|---|---|---|---|
| **Stack** | 1-D flex (the workhorse) | `direction "row"\|"column"`, `gap: Gap`, `align`, `justify`, `wrap?`, `asChild` | `display:flex`. No surface paint. |
| **Grid** | 2-D responsive grid | `columns: number \| {sm,md,lg}`, `gap: Gap`, `align`, `flow?`, `minColWidth?` (→ `auto-fit minmax`) | `minColWidth` enables intrinsic responsive without media queries. |
| **Cluster** | wrap-flow row of items (pill/tag/action groups) | `gap: Gap`, `align`, `justify` | Distinct from `Toolbar` primitive: Toolbar = interactive roving-tabindex group; Cluster = pure visual wrap. |
| **Container** | page max-width + horizontal centering; replaces `PageFrame` width logic | `size "sm"\|"md"\|"lg"\|"xl"\|"full"`, `padX?: Pad`, `asChild` | The only width authority; carries gutter padding. |
| **Split** / **Sidebar** | two-pane (fixed side + fluid main); underlies AppShell | `side "start"\|"end"`, `sideWidth`, `gap: Gap`, `collapseBelow?` | One component, `Split.Side` + `Split.Main` parts. |
| **Center** | center content in both axes | `inline?`, `minHeight?`, `asChild` | Used by EmptyState/loading. |

### 4b · Layout compositions (built from 4a)

| Piece | Purpose / generalizes | Compound parts | Notes |
|---|---|---|---|
| **AppShell** | app frame: header / sidebar / main / optional footer; generalizes `WorkspaceShell` | `AppShell`, `.Header`, `.Sidebar`, `.Main`, `.Footer` | Sidebar collapsible via `sidebarOpen`/`onSidebarOpenChange` (controlled+uncontrolled). Built from `Split`. Skip-to-content link baked in for a11y. |
| **PageHeader** | page title band; generalizes `PageFrame`/`TopBar` header | `PageHeader`, `.Breadcrumbs`, `.Title`, `.Description`, `.Actions` | Title is `<h1>` default, `asChild` to relevel. Actions cluster right. |

### 4c · Blocks

| Piece | Purpose / generalizes | API sketch | Test focus |
|---|---|---|---|
| **EmptyState** | zero-data surface; generalizes local `EmptyState` | parts: `.Icon`, `.Title`, `.Description`, `.Actions`; or ergonomic `title`/`description`/`action` props | renders, axe; role/heading semantics |
| **ErrorState** | error/failure surface; generalizes `ErrorState` | `intent "error"\|"warning"`, `.Title`/`.Description`/`.Actions`, optional `onRetry` | retry `play()`; `role="alert"` only when live |
| **Skeleton** | loading placeholder; generalizes `Skeleton` | `variant "text"\|"rect"\|"circle"`, `lines?`, `width/height` | `aria-hidden`; reduced-motion disables shimmer |
| **Spinner** | indeterminate busy; generalizes `Spinner` | `size`, `label` (visually-hidden) | reduced-motion; `role="status"` + label |
| **Badge** | status/count token; **promotes the placeholder to real** | `intent`, `variant "solid"\|"soft"\|"outline"`, `size` | replaces `placeholders.tsx` Badge; remove placeholder export |
| **Tag** / **Chip** | removable label; generalizes `Pill`/`FilterPill` | `removable?`, `onRemove`, `selected?` (filter), `size` | remove-button `play()`; keyboard remove (⌫/Del) |
| **StatCard** | KPI/metric card; generalizes `Kpi` | `label`, `value`, `delta? {value, direction}`, `icon?`, built on `Card` | delta sign/color; axe contrast |
| **Banner** / **Callout** | inline page-level message; generalizes `LiveBanner` | `intent`, `dismissible?`, `onDismiss`, `.Title`/`.Description`/`.Actions` | dismiss `play()`; `role` per intent |
| **DescriptionList** | key→value rows; generalizes `LedgerRow`/`Receipt` | `<DescriptionList>` + `.Item`/`.Term`/`.Detail`; `orientation`, `divider?` | semantic `<dl>/<dt>/<dd>` |
| **DataTable** | the long-promised `Table` (presentational) | `columns: Column<T>[]`, `data: T[]`, `sort?`, `onSortChange`, `selection?`, `rowKey`, `stickyHeader?`, `density`, render slots for empty/loading | **own long pole** — see §5 wave 3 |

---

## 5 · Sequencing (waves)

Each piece is one `component-slice-flow` slice (brief → implement → dual-review
→ fix → visual-review → polish → commit). Waves gate on the prior wave being
green so the conventions are proven before they scale.

- **Wave 1 — Layout primitives:** `Stack`, `Grid`, `Cluster`, `Container`,
  `Split`, `Center`. Establishes `Gap`/`Pad`/`Align`/`Justify` shared unions
  and the `src/layouts/` subtree. (Highest leverage; everything else composes
  these.)
- **Wave 2 — Compositions + small blocks:** `AppShell`, `PageHeader`,
  `EmptyState`, `ErrorState`, `Skeleton`, `Spinner`, `Badge` (promote),
  `Tag`/`Chip`, `StatCard`, `Banner`, `DescriptionList`.
- **Wave 3 — DataTable:** dedicated slice. Decisions to settle in its brief:
  sortable-header keyboard model, row selection (checkbox + `aria-selected`),
  sticky header, density, empty/loading/error slot wiring, and whether
  virtualization is in or explicitly deferred. Presentational only.

---

## 6 · Testing

- Per-piece Storybook stories are the spec-of-record (CONVENTIONS.md).
- Smoke + axe on every story via the Test Runner; `play()` only where behavior
  beats markup (dismiss, remove, sort, sidebar toggle, retry).
- Reduced-motion paths asserted for `Skeleton`/`Spinner` (matches the
  Progress/Separator wave-10 precedent).
- Layout primitives: assert the produced CSS contract (flex/grid + token-mapped
  gap), not pixel geometry.

---

## 7 · Risks

- **Scope.** 18 pieces (6 primitives + 2 compositions + 10 blocks) is a large
  round; the wave gates and the slice flow
  keep each reviewable. DataTable is isolated so it can't stall the rest.
- **Layout-token leakage.** Arbitrary spacing would defeat governance — the
  closed `Gap`/`Pad` unions prevent it; review every layout CSS for raw values.
- **Cluster vs Toolbar confusion.** Documented split in §4a; Cluster JSDoc must
  state "non-interactive; use Toolbar for roving focus."
- **AppShell portal/theme.** Theme host is `<html>` (decision doc) — AppShell
  must not re-scope `data-theme`; verify overlays opened from within inherit.

---

## 8 · Deferred (explicit)

- Builder migration: delete local duplicates, rewire builder surfaces.
- Full page templates.
- Atelier↔crystal decision-doc reconciliation.
- `DataTable` virtualization (decide in its brief; default: deferred).
- Domain cards (`ProjectCard`/`TemplateCard`) stay builder-local thin wrappers
  over the governed `Card`/`StatCard`.
