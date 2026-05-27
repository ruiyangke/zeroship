# Slice 3 — Card + Dialog + AlertDialog (HIG-anchored, library-survey informed)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `818b2122`).
**Status when this brief is written:** slices 1 + 2 shipped + polished.
Real components: Button, Input, Field. Placeholders remaining: Badge,
Card, Dialog. Slice 3 lands Card + Dialog + AlertDialog (a separate
component per HIG and W3C APG conventions).

## Goal

Replace the Card + Dialog placeholders with real HIG-anchored
components, plus a NEW `AlertDialog` (separate component with HIG
button-layout rules baked in structurally). Apply slice 1's
glass-surface pattern in production for the first time. Migrate the
builder's `SettingsCanvas DeleteConfirm` from placeholder Dialog to
real AlertDialog.

THREE components (Card / Dialog / AlertDialog), token additions, the
`--zs-blur` rename, and the builder migration. No Sheet/Drawer, no
Popover, no Toast — those are slice 4+.

## Hard constraints (unchanged)

- Worktree single-writer.
- Pre-launch, no back-compat. Rename `--zs-blur` → `--zs-material-regular`
  in styles.css AND every consumer (story.css; any future code). No alias.
- Plain CSS + `--zs-*` tokens.
- No raw hex, no raw px in CSS (including comments — slice-2 review caught 3).
- HIG anchor; Base UI is the headless layer; never overwrite its aria wiring.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via
  logical properties — mandatory.
- DO NOT commit, push, or merge.

## HIG citations

- `https://developer.apple.com/design/human-interface-guidelines/boxes` —
  HIG Boxes are our Card. Keep boxes small vs containing view. Optional
  succinct title.
- `https://developer.apple.com/design/human-interface-guidelines/sheets` —
  Simple content/tasks; one sheet at a time; iOS grabber/swipe-to-dismiss;
  Done/Cancel positions.
- `https://developer.apple.com/design/human-interface-guidelines/alerts` —
  Use sparingly. Direct neutral tone. Title + optional text + button stack.
  Don't alert for common undoable actions.
- `https://developer.apple.com/design/human-interface-guidelines/materials` —
  Material thickness vocabulary (ultra-thin / thin / regular / thick /
  ultra-thick); thicker = more legible, thinner = more contextual.

## Library-survey takeaways (full report: `/tmp/card-dialog-research.md`)

- **Card**: decomposition wins (shadcn/Chakra/Park UI/Fluent agree).
  Monolithic Card (Radix Themes, Mantine, Geist) needs escape hatches
  anyway. We decompose into Header/Title/Description/Action/Media/Body/
  Footer.
- **Dialog**: subparts mirror Base UI's headless shape directly.
  Layout sublayers (Header/Body/Footer) are layout-only divs (shadcn
  pattern) — no aria semantics there; Title and Description carry ARIA.
- **AlertDialog**: shipped as a SEPARATE component (Base UI, Radix,
  shadcn, Chakra). Encodes HIG button-layout rules structurally
  (1-button full-width / 2-button side-by-side / 3+ button stacked) so
  consumers don't have to remember them.
- **15 anti-patterns** from the survey — we explicitly avoid every one:
  1. Cards with `::before`/`::after` overlays (axe-glass rule).
  2. Whole-card-clickable via `<a>` wrapper (breaks nested interactives).
  3. Auto-X close button absolutely positioned outside the header.
  4. Per-button props on Modal (`primaryAction={{...}}`).
  5. Dialog with no `role` distinction between alert and regular.
  6. AlertDialog dismissible by outside-click.
  7. Dialog without focus restore.
  8. Static methods (`Modal.confirm()`).
  9. Backdrop blur baked in with no opaque base.
  10. `min-height: 100vh` (use `100dvh`).
  11. No `aria-describedby` on dialogs with descriptions.
  12. Disabling outside-click but not ESC.
  13. Multiple primary actions in AlertDialog (dev-warn).
  14. Card padding misaligned with `--zs-space-*`.
  15. `Card.Section` negative-margin trick without `overflow: hidden` clip.

## Files to create

- `sdks/ui/src/components/Card/Card.tsx`
- `sdks/ui/src/components/Card/Card.css`
- `sdks/ui/src/components/Card/index.ts`
- `sdks/ui/src/components/Dialog/Dialog.tsx`
- `sdks/ui/src/components/Dialog/Dialog.css`
- `sdks/ui/src/components/Dialog/index.ts`
- `sdks/ui/src/components/AlertDialog/AlertDialog.tsx`
- `sdks/ui/src/components/AlertDialog/AlertDialog.css`
- `sdks/ui/src/components/AlertDialog/index.ts`
- `sdks/ui/src/stories/Card.stories.tsx`
- `sdks/ui/src/stories/Dialog.stories.tsx`
- `sdks/ui/src/stories/AlertDialog.stories.tsx`
- `sdks/ui/scripts/capture-card-evidence.mjs`
- `sdks/ui/scripts/capture-dialog-evidence.mjs`
- `sdks/ui/scripts/capture-alertdialog-evidence.mjs`

## Files to modify

- `sdks/ui/src/styles.css` — **rename `--zs-blur` → `--zs-material-regular`**;
  add the full material thickness scale, shadow scale, card/dialog dims.
  Add the new tokens to the crystal palette block.
- `sdks/ui/src/components/index.ts` — re-export Card, Dialog, AlertDialog
  and their prop/subpart types.
- `sdks/ui/src/index.ts` — drop Card / Dialog from placeholders re-export;
  add from `./components`. Badge stays in placeholders.
- `sdks/ui/src/placeholders.tsx` — DELETE Card export, CardProps,
  Dialog export, DialogProps. Keep Badge + BadgeProps.
- `sdks/ui/src/stories/story.css` — replace `var(--zs-blur)` with
  `var(--zs-material-regular)` in the documented places.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — append the new story IDs
  for Card / Dialog / AlertDialog.
- `apps/zeroship-builder/src/client/workspace/canvases/SettingsCanvas.tsx`
  — migrate `DeleteConfirm` from placeholder `<Dialog open …>` to
  decomposed AlertDialog. See "Builder migration" section below.

## Foundation tokens — `:root` additions in styles.css

(All theme-invariant. Add to existing `:root`.)

```css
:root {
  /* (slice 1 + 2 tokens unchanged) */

  /* ─── Material thicknesses — HIG vocabulary ─────────────────────────── */
  --zs-material-ultra-thin: blur(0.5rem) saturate(1.6);
  --zs-material-thin:       blur(1rem) saturate(1.6);
  --zs-material-regular:    blur(1.5rem) saturate(1.8);   /* surfaces default */
  --zs-material-thick:      blur(2rem) saturate(1.8);     /* backdrop default */
  --zs-material-ultra-thick: blur(2.5rem) saturate(2);

  /* ─── Shadow scale — elevation as a function of depth ───────────────── */
  /* All shadows use oklch alpha; no rgb literals. */
  --zs-shadow-1: 0 0.0625rem 0.125rem oklch(0 0 0 / 0.06),
                 0 0.0625rem 0.125rem oklch(0 0 0 / 0.04);
  --zs-shadow-2: 0 0.25rem 0.5rem oklch(0 0 0 / 0.08),
                 0 0.125rem 0.25rem oklch(0 0 0 / 0.06);
  --zs-shadow-3: 0 0.5rem 1rem oklch(0 0 0 / 0.1),
                 0 0.25rem 0.5rem oklch(0 0 0 / 0.06);
  --zs-shadow-4: 0 1rem 2rem oklch(0 0 0 / 0.12),
                 0 0.5rem 1rem oklch(0 0 0 / 0.08);
  --zs-shadow-dialog: 0 1.5rem 4rem oklch(0 0 0 / 0.35),
                     0 0.5rem 1.5rem oklch(0 0 0 / 0.18);

  /* ─── Card sizing ───────────────────────────────────────────────────── */
  --zs-card-radius-sm: var(--zs-radius-4);   /* 10 */
  --zs-card-radius-md: var(--zs-radius-5);   /* 12 — default */
  --zs-card-radius-lg: var(--zs-radius-6);   /* 16 */
  --zs-card-padding-sm: var(--zs-space-4);   /* 16 */
  --zs-card-padding-md: var(--zs-space-6);   /* 24 — default */
  --zs-card-padding-lg: var(--zs-space-7);   /* 32 */
  --zs-card-gap-sm: var(--zs-space-3);       /* 12 */
  --zs-card-gap-md: var(--zs-space-4);       /* 16 */
  --zs-card-gap-lg: var(--zs-space-5);       /* 20 */

  /* ─── Dialog sizing (max-widths converted from research's px) ───────── */
  --zs-dialog-radius: var(--zs-radius-7);            /* 22 — standard sheet */
  --zs-dialog-radius-alert: var(--zs-radius-5);      /* 12 — tighter for alerts */
  --zs-dialog-max-width-sm: 22.5rem;                 /* 360 */
  --zs-dialog-max-width-md: 32.5rem;                 /* 520 — default */
  --zs-dialog-max-width-lg: 45rem;                   /* 720 */
  --zs-dialog-padding: var(--zs-space-6);            /* 24 */
  --zs-dialog-padding-alert: var(--zs-space-5);      /* 20 */
}
```

**The `--zs-blur` token from slice 1 is removed**. Every consumer
(story.css; the crystal palette declares `--zs-blur` — REPLACE with
`--zs-material-regular`) updates in this same commit.

## Crystal theme token additions — `[data-theme="crystal"]` block

```css
[data-theme="crystal"] {
  /* (existing slice-1/2 entries unchanged EXCEPT --zs-blur is removed) */

  /* Material — replaces the old --zs-blur (pre-launch, no alias) */
  --zs-material-regular: blur(1.5rem) saturate(1.25);

  /* Scrim for the standard Dialog backdrop */
  --zs-scrim: color-mix(in oklch, black 40%, transparent);

  /* Material tint for the glass Dialog backdrop */
  --zs-material-tint: color-mix(in oklch, var(--zs-surface) 80%, transparent);
}
```

(The crystal theme's saturate value is lighter than the root default —
Crystal is the light glass, so a less intense saturation is gentler. The
root token defaults to a saturate suitable for darker themes.)

## Card — API

Decomposed subparts. Mirrors shadcn/Chakra/Park UI/Fluent shape.

```tsx
export type CardVariant = "surface" | "elevated" | "outline" | "ghost";
export type CardSize = "sm" | "md" | "lg";

export interface CardProps extends ComponentPropsWithoutRef<"div"> {
  variant?: CardVariant;       // default "surface"
  size?: CardSize;             // default "md"
  /** Apply hover/focus/active states even on non-asChild. */
  interactive?: boolean;
  /** Render-as: <Card asChild><a href=…> for whole-card clickable. */
  asChild?: boolean;
  children?: ReactNode;
}

// Subparts (all forwardRef, all with composeBaseClass-pattern className):
Card.Header    : layout row — flex; gap; aligns Title/Description left,
                  Action right via margin-inline-start: auto.
Card.Title     : <h3> by default (asChild lets you swap to h2/h4).
                  font-size = title-3 at md, title-2 at lg, headline at sm.
Card.Description : muted subtitle. font-size = subheadline at md/lg,
                   footnote at sm.
Card.Action    : top-right slot inside Header (or anywhere else inside
                  Card, but Header is the canonical home).
Card.Media     : edge-bleed media slot. Prop `side: "top" | "bottom" |
                  "left" | "right" | "fill"` (default "top").
Card.Body      : main content.
Card.Footer    : button row; right-aligned by default; supports
                  `align="start" | "between" | "end"`.
```

Implementation notes:
- Card root: `overflow: hidden` so `Card.Media` clips to corner radius.
- `asChild` uses the inline Slot from slice 1 (Button.tsx already has
  Slot helpers — copy or extract to a shared `components/_slot.ts`).
  EXTRACTION recommendation: move Slot helpers from Button.tsx into
  `sdks/ui/src/components/_slot.ts` so Card / Dialog / future
  components all import from there. Update Button.tsx import.
- `interactive` adds focus-ring + hover bg shift; does NOT add an
  onClick handler (consumer's responsibility).
- Variants:
  - `surface`: opaque background (`var(--zs-surface)`), no border, no shadow.
  - `elevated`: opaque + `--zs-shadow-2`.
  - `outline`: opaque + `box-shadow: 0 0 0 0.0625rem var(--zs-separator) inset`.
  - `ghost`: TRANSPARENT — the only Card that opts out of opaque-base.
    Meant for nesting inside an already-opaque surface; documented in code.

## Dialog — API

Subparts mirror Base UI's headless shape. Layout sublayers
(Header/Body/Footer) are layout-only divs.

```tsx
export interface DialogProps {
  open?: boolean;                     // controlled
  defaultOpen?: boolean;              // uncontrolled
  onOpenChange?: (
    open: boolean,
    eventDetails?: { reason?: string }
  ) => void;
  modal?: boolean;                    // default true
  /** false = no outside-click + no ESC dismissal. */
  dismissible?: boolean;              // default true
  children?: ReactNode;
}

Dialog.Trigger                     // Base UI passthrough
Dialog.Portal                      // Base UI passthrough
Dialog.Backdrop                    // takes `tint?: "scrim" | "material" | "none"` (default "scrim")
Dialog.Popup                       // takes `size?`, `placement?`, `showClose?`, `initialFocus?`, `finalFocus?`
Dialog.Header                      // layout: Title + Description + auto-X close right
Dialog.Title                       // Base UI passthrough; <h2> by default
Dialog.Description                 // Base UI passthrough; <p>; auto-feeds aria-describedby
Dialog.Body                        // scroll container; padding from --zs-dialog-padding
Dialog.Footer                      // button row; right-aligned default
Dialog.Close                       // Base UI passthrough; styled as a Button under the hood
```

Popup props:
- `size: "sm" | "md" | "lg" | "full"` (default `"md"`).
- `placement: "center" | "top"` (default `"center"`).
- `showClose: boolean` (default `true`) — render auto-X in Header.
- Base UI's `initialFocus` / `finalFocus` pass through.

Backdrop props:
- `tint: "scrim" | "material" | "none"` (default `"scrim"`).

## AlertDialog — API

Separate component (W3C APG + HIG convention). Mirrors Dialog's
subparts EXCEPT:
- `dismissible` is forced to `false` (HIG: alerts require an action).
  Outside-click never dismisses. ESC dismisses by closing
  AlertDialog.Cancel if present, otherwise nothing.
- `AlertDialog.Popup` defaults to `size="sm"`, `--zs-dialog-radius-alert`
  (`--zs-radius-5`), `--zs-dialog-padding-alert`.
- `AlertDialog.Footer` **auto-arranges children** per HIG:
  - 1 button → full-width.
  - 2 buttons → side-by-side; Cancel left, Action right.
  - 3+ buttons → stacked vertically; destructive at the bottom.
- `AlertDialog.Cancel` and `AlertDialog.Action` are styled wrappers
  around our Button:
  - `Cancel`: `<Button variant="gray">{children}</Button>` + auto-close
    on click.
  - `Action`: `<Button variant="filled" intent={tone || "normal"}>` +
    auto-close on click (unless `preventClose` prop is set).
- `Action` accepts `tone="destructive"` to flip to `intent="destructive"`.

Dev warning (only `process.env.NODE_ENV !== "production"`): if
AlertDialog.Footer contains > 1 AlertDialog.Action with no
`tone="destructive"` distinguishing them, console.warn.

## Builder migration — SettingsCanvas.tsx DeleteConfirm

Current state uses the placeholder Dialog API:
```tsx
<Dialog open={confirmOpen} onOpenChange={setConfirmOpen}
        title="Delete this app?"
        footer={<><Button variant="plain" role="cancel">cancel</Button>
                   <Button variant="filled" role="destructive">delete</Button></>}>
  <p>This permanently deletes …</p>
  <Input value={typed} onChange={…} />
</Dialog>
```

Migrate to AlertDialog (correct semantically — it's confirm-destructive):
```tsx
<AlertDialog open={confirmOpen} onOpenChange={setConfirmOpen}>
  <AlertDialog.Portal>
    <AlertDialog.Backdrop />
    <AlertDialog.Popup>
      <AlertDialog.Header>
        <AlertDialog.Title>Delete this app?</AlertDialog.Title>
        <AlertDialog.Description>
          This permanently deletes the app and all of its data.
          Type the app name to confirm.
        </AlertDialog.Description>
      </AlertDialog.Header>
      <AlertDialog.Body>
        <Input value={typed} onChange={…} aria-label="Type app name" />
      </AlertDialog.Body>
      <AlertDialog.Footer>
        <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
        <AlertDialog.Action
          tone="destructive"
          disabled={typed !== appName}
          onClick={onConfirm}
        >
          Delete
        </AlertDialog.Action>
      </AlertDialog.Footer>
    </AlertDialog.Popup>
  </AlertDialog.Portal>
</AlertDialog>
```

The Cancel button auto-closes via AlertDialog.Cancel; Delete is gated
by the type-match (Action's `disabled` prop).

## Stories — comprehensive coverage

### Card.stories.tsx — title `Components/Card`

1. **AllVariants** — surface / elevated / outline / ghost side-by-side, md size.
2. **AllSizes** — sm / md / lg side-by-side, surface variant.
3. **Decomposed** — Card with Header (Title + Description + Action), Body, Footer (with two Buttons).
4. **WithMedia** — Card with Card.Media side="top" (a colored placeholder div, not a real image — keep stories self-contained).
5. **Interactive** — Card with `interactive` prop; hover state visible.
6. **AsChild** — `<Card asChild><a href="#">…</a></Card>`; entire card is a link.
7. **Ghost** — Ghost variant nested inside a surface Card; demonstrates the intended use.
8. **WithFormInside** — Card containing a Field + Input + Button — exercises the cross-slice composition.

### Dialog.stories.tsx — title `Components/Dialog`

1. **Default** — Trigger button + standard md Dialog with Header/Body/Footer.
2. **Sizes** — sm / md / lg / full triggers.
3. **PlacementTop** — `placement="top"` for iPad form-sheet feel.
4. **BackdropTints** — three side-by-side triggers showing scrim / material / none backdrops.
5. **WithForm** — Dialog containing a Field + Input + Submit; demonstrates focus-trap on a form.
6. **NonDismissible** — `dismissible={false}` Dialog; outside-click and ESC both ignored; Close button required.
7. **InitialFocus** — focus lands on a specific input inside the Dialog on open.
8. **Nested** — Dialog opens another Dialog; shows nested-dialogs scaling (use the `--nested-dialogs` CSS var).

### AlertDialog.stories.tsx — title `Components/AlertDialog`

1. **OneButton** — AlertDialog with only AlertDialog.Action ("OK") — full-width footer.
2. **TwoButtons** — Cancel + Action — side-by-side footer; standard pattern.
3. **Destructive** — Cancel + Action with `tone="destructive"` — red action button.
4. **ThreeButtons** — three actions; auto-stacked vertically; destructive at bottom.
5. **WithBody** — alert with a body input field (e.g., "type the name to confirm" pattern from SettingsCanvas).
6. **OutsideClickIgnored** — story description tells reviewer to try clicking outside; nothing happens. (No assertion needed; visual demonstration.)

Update `scripts/check-storybook-a11y.mjs` to include all new story IDs.
8 + 8 + 6 = 22 new stories. Total: 30 (Button/Input) + 22 = 52 stories
across 1 theme.

Update the three capture scripts (one per component) — they each
mirror `capture-button-evidence.mjs` / `capture-input-evidence.mjs`
shape.

## CSS structure (sketch)

### Card.css

```css
.zs-card {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: var(--zs-card-gap-md);
  padding: var(--zs-card-padding-md);
  border-radius: var(--zs-card-radius-md);
  background-color: var(--zs-surface);
  backdrop-filter: var(--zs-material-regular);
  -webkit-backdrop-filter: var(--zs-material-regular);
  color: var(--zs-label);
  overflow: hidden;                  /* clip Card.Media to radius */
}

.zs-card--sm { padding: var(--zs-card-padding-sm); gap: var(--zs-card-gap-sm); border-radius: var(--zs-card-radius-sm); }
.zs-card--lg { padding: var(--zs-card-padding-lg); gap: var(--zs-card-gap-lg); border-radius: var(--zs-card-radius-lg); }

.zs-card--elevated { box-shadow: var(--zs-shadow-2); }
.zs-card--outline  { box-shadow: 0 0 0 0.0625rem var(--zs-separator) inset; }
.zs-card--ghost {
  background-color: transparent;
  backdrop-filter: none;
  -webkit-backdrop-filter: none;
}

.zs-card[data-interactive] { transition: transform var(--zs-motion-fast) var(--zs-motion-spring), background-color var(--zs-motion-fast) var(--zs-motion-ease); }
@media (hover: hover) {
  .zs-card[data-interactive]:hover { background-color: var(--zs-fill-quaternary); }
}
.zs-card[data-interactive]:focus-visible { outline: var(--zs-focus-ring-width) solid var(--zs-focus-ring-color); outline-offset: var(--zs-focus-ring-offset); }
.zs-card[data-interactive]:active { transform: scale(0.99); }

.zs-card__header { display: flex; gap: var(--zs-space-3); align-items: flex-start; }
.zs-card__header > .zs-card__action { margin-inline-start: auto; }
.zs-card__title { font-size: var(--zs-text-title-3-size); line-height: var(--zs-text-title-3-line); font-weight: 600; }
.zs-card__description { font-size: var(--zs-text-subheadline-size); line-height: var(--zs-text-subheadline-line); color: var(--zs-label-secondary); }
.zs-card__media[data-side="top"]    { margin-inline: calc(var(--zs-card-padding-md) * -1); margin-block-start: calc(var(--zs-card-padding-md) * -1); }
.zs-card__media[data-side="bottom"] { margin-inline: calc(var(--zs-card-padding-md) * -1); margin-block-end: calc(var(--zs-card-padding-md) * -1); }
.zs-card__media[data-side="fill"]   { position: absolute; inset: 0; z-index: 0; pointer-events: none; }
.zs-card > :not(.zs-card__media[data-side="fill"]) { position: relative; z-index: 1; }
.zs-card__body { display: flex; flex-direction: column; gap: var(--zs-space-3); }
.zs-card__footer { display: flex; gap: var(--zs-space-2); align-items: center; }
.zs-card__footer[data-align="end"]     { justify-content: flex-end; }
.zs-card__footer[data-align="start"]   { justify-content: flex-start; }
.zs-card__footer[data-align="between"] { justify-content: space-between; }

/* forced-colors */
@media (forced-colors: active) {
  .zs-card { background: Canvas; color: CanvasText; box-shadow: 0 0 0 0.0625rem CanvasText inset; }
  .zs-card--ghost { background: transparent; box-shadow: none; }
}
```

### Dialog.css

```css
.zs-dialog-backdrop {
  position: fixed; inset: 0;
  display: grid; place-items: center;
  z-index: var(--zs-z-modal, 50);
  /* default scrim — both tinted variants override */
  background-color: var(--zs-scrim);
}
.zs-dialog-backdrop[data-tint="material"] {
  background-color: var(--zs-material-tint);
  backdrop-filter: var(--zs-material-thick);
  -webkit-backdrop-filter: var(--zs-material-thick);
}
.zs-dialog-backdrop[data-tint="none"] { background-color: transparent; }

.zs-dialog-popup {
  position: relative;
  display: flex;
  flex-direction: column;
  inline-size: 100%;
  max-inline-size: var(--zs-dialog-max-width-md);
  max-block-size: calc(100dvh - var(--zs-space-7) * 2);
  margin: var(--zs-space-7);
  background-color: var(--zs-surface);
  backdrop-filter: var(--zs-material-regular);
  -webkit-backdrop-filter: var(--zs-material-regular);
  border-radius: var(--zs-dialog-radius);
  box-shadow: var(--zs-shadow-dialog);
  overflow: hidden;
}
.zs-dialog-popup[data-size="sm"] { max-inline-size: var(--zs-dialog-max-width-sm); }
.zs-dialog-popup[data-size="lg"] { max-inline-size: var(--zs-dialog-max-width-lg); }
.zs-dialog-popup[data-size="full"] { max-inline-size: none; max-block-size: 100dvh; border-radius: 0; margin: 0; }

.zs-dialog-popup[data-placement="top"] {
  align-self: flex-start;
  margin-block-start: max(var(--zs-space-9), env(safe-area-inset-top));
}

/* nested-dialogs scaling */
.zs-dialog-popup {
  --zs-nested-scale: calc(1 - var(--nested-dialogs, 0) * 0.02);
  transform: scale(var(--zs-nested-scale)) translateY(calc(var(--nested-dialogs, 0) * 0.5rem));
  transition: transform var(--zs-motion-base) var(--zs-motion-ease);
}
@media (prefers-reduced-motion: reduce) {
  .zs-dialog-popup { transform: none; transition: none; }
}

/* layout sublayers */
.zs-dialog__header {
  display: flex; align-items: flex-start; gap: var(--zs-space-3);
  padding: var(--zs-dialog-padding); padding-block-end: var(--zs-space-3);
}
.zs-dialog__header-content { flex: 1; }
.zs-dialog__header-close { margin-inline-start: auto; }
.zs-dialog__title { font-size: var(--zs-text-title-3-size); line-height: var(--zs-text-title-3-line); font-weight: 600; }
.zs-dialog__description { font-size: var(--zs-text-subheadline-size); line-height: var(--zs-text-subheadline-line); color: var(--zs-label-secondary); margin-block-start: var(--zs-space-1); }
.zs-dialog__body { padding: 0 var(--zs-dialog-padding); flex: 1; overflow-y: auto; min-block-size: 0; }
.zs-dialog__footer {
  display: flex; gap: var(--zs-space-2); align-items: center; justify-content: flex-end;
  padding: var(--zs-dialog-padding);
  padding-block-start: var(--zs-space-4);
}

/* Base UI emits data-starting-style / data-ending-style on Popup. */
.zs-dialog-popup[data-starting-style] { opacity: 0; transform: scale(0.96) translateY(0.5rem); }
.zs-dialog-popup[data-ending-style]   { opacity: 0; transform: scale(0.98) translateY(0.25rem); }
.zs-dialog-backdrop[data-starting-style], .zs-dialog-backdrop[data-ending-style] { opacity: 0; }
@media (prefers-reduced-motion: reduce) {
  .zs-dialog-popup[data-starting-style],
  .zs-dialog-popup[data-ending-style] { transform: none; }
}

/* forced-colors */
@media (forced-colors: active) {
  .zs-dialog-popup { background: Canvas; color: CanvasText; box-shadow: 0 0 0 0.0625rem CanvasText inset; }
  .zs-dialog-backdrop[data-tint="scrim"],
  .zs-dialog-backdrop[data-tint="material"] { background-color: color-mix(in oklch, CanvasText 40%, transparent); backdrop-filter: none; }
}
```

### AlertDialog.css

Inherits Dialog.css's layout. Override only what differs:

```css
.zs-alertdialog-popup {
  /* size sm by default; radius and padding overridden */
  max-inline-size: var(--zs-dialog-max-width-sm);
  border-radius: var(--zs-dialog-radius-alert);
}
.zs-alertdialog__footer {
  /* one-button layout: full-width */
  display: grid;
  gap: var(--zs-space-2);
  padding: var(--zs-dialog-padding-alert);
  padding-block-start: var(--zs-space-3);
}
.zs-alertdialog__footer[data-button-count="1"] { grid-template-columns: 1fr; }
.zs-alertdialog__footer[data-button-count="2"] { grid-template-columns: 1fr 1fr; }
.zs-alertdialog__footer[data-button-count="3+"] { grid-template-columns: 1fr; }    /* stacked */
.zs-alertdialog__footer[data-button-count="3+"] > * { inline-size: 100%; }
```

The footer's data-button-count is set by `AlertDialog.Footer`
imperatively — it counts its children at render time
(`Children.count(children)` from React).

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity (incl. comments):
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty.
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty.
   - **VERIFY `--zs-blur` is gone**: `grep -rn 'zs-blur' sdks/ui apps/zeroship-builder --include='*.css' --include='*.tsx' --include='*.ts'` → empty.
4. `pnpm --filter zeroship-builder build` → green. The SettingsCanvas
   migration to AlertDialog must compile and behave.
5. **A11y violations**: serve storybook-static, run
   `STORYBOOK_URL=… node sdks/ui/scripts/check-storybook-a11y.mjs`.
   Must print `A11y clean for 52 stories across 1 themes (no serious/critical violations)`.
6. **A11y incomplete sweep**: write a temp scanner; 0 background-gradient,
   0 pseudo-element incompletes across all 52 stories. The new
   `--zs-material-tint` Backdrop (color-mix translucent) — verify axe
   doesn't flag descendants (Backdrop has no descendants except the
   Popup which has its own opaque surface).
7. **Aria wiring assertions** for Dialog + AlertDialog:
   - Dialog `<role="dialog">`, `aria-labelledby` ref Title id, `aria-describedby`
     ref Description id when present.
   - AlertDialog `<role="alertdialog">`, same labelling + describing.
   - Focus trap: Tab cycles within Popup; Shift+Tab cycles backward.
   - ESC dismisses when `dismissible=true`; doesn't when `false`.
   - AlertDialog: outside-click does NOT dismiss; ESC closes Cancel if present.
   - Focus restored to Trigger on close.
   - Extend `scripts/check-aria-wiring.mjs` to cover Dialog + AlertDialog
     using playwright; print pass/fail per assertion.
8. Capture evidence: run all three new capture scripts; verify all 22
   PNGs exist.

## Report (stdout)

End with:
- Files changed/created.
- Per-decision confirmation (Card subparts ✓, Dialog subparts ✓,
  AlertDialog auto-footer-layout ✓, `--zs-blur` removed ✓,
  SettingsCanvas migrated to AlertDialog ✓, all 15 anti-patterns
  avoided ✓, forced-colors rules ✓, RTL ✓, nested-dialogs scaling ✓).
- Token purity grep results.
- Build status (both packages).
- A11y violations line (52 stories × 1 theme).
- A11y incomplete result (zero gradient + zero pseudo).
- Aria wiring assertion results.
- Screenshot paths — all 22 new PNGs.
- One taste note (if any).
- Explicit: "I did NOT commit, push, or merge."
