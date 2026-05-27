# Slice 3 Card — review fixes (no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `de7ea7e5`).
Lands every finding from three independent review passes: pilot self-review,
codex (gpt-5.5 xhigh) read-only second pass, and claude (opus) code-reviewer
third pass. 26 items, nothing deferred per the established slice-1/2 pattern.

## Goal
One focused commit that closes every Card-specific item. Builds + a11y
verifier remain green. Stories matrix expands to demonstrate the
fixed behaviors. Does NOT touch Dialog or AlertDialog except where the
shared Slot/composeRefs change ripples there.

## Hard constraints (unchanged)
- Worktree single-writer.
- Pre-launch, no back-compat — rename / restructure freely.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex, no raw px (oklch + rem only; including comments).
- HIG anchor; Base UI is the headless layer; never overwrite its aria wiring.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via
  logical properties — mandatory.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (8)

**1. `interactive` makes a `<div>` focusable with no keyboard activation.**
`Card.tsx:75-82` + `Card.css:70-91`. Setting `tabIndex={0}` on a div is
a "fake control": Tab lands there, focus ring shows, Enter/Space do
nothing. Two acceptable fixes; pick (a):
- **(a) [recommended]** When `interactive={true}` AND NOT `asChild`,
  ALSO set `role="button"` AND add `onKeyDown` that forwards
  Enter/Space to `onClick`. If the consumer didn't pass `onClick`,
  `interactive` becomes a no-op visual modifier with a dev-mode
  console.warn ("Card interactive=true but no onClick handler;
  keyboard users can't activate it. Use asChild with a real
  link/button if you don't want onClick.").
- (b) Require `asChild` for interactive cards. Reject `interactive`
  without `asChild` with a dev-mode error.

Add a story `InteractiveWithKeyboard` that exercises Enter + Space
on a focused interactive card and asserts onClick fires (extend
`check-aria-wiring.mjs`).

**2. Hover swaps opaque `--zs-surface` for a 6%-alpha translucent fill.**
`Card.css:78-80`. The component-header comment EXPLICITLY says "every
variant except `ghost` declares an OPAQUE background-color so axe's
color-contrast walk can resolve at the card boundary." Hover violates
this. Replace the background-color swap with an opaque overlay:
```css
@media (hover: hover) {
  .zs-card[data-interactive]:hover {
    /* Layer a translucent tint OVER the opaque base instead of replacing.
       The base-color remains computable for axe; visually a subtle
       darken matches HIG list-row hover. */
    background-image: linear-gradient(var(--zs-card-hover-tint),
                                       var(--zs-card-hover-tint));
  }
}
```
Add `--zs-card-hover-tint` to the crystal palette block:
`--zs-card-hover-tint: color-mix(in oklch, var(--zs-label) 4%, transparent);`
(4% label-over-surface reads as a barely-visible darken, matches macOS
list hover.) Run the a11y-incomplete sweep with the Interactive story
under simulated `:hover` and confirm no new gradient incompletes.

**3. Header layout: switch to CSS Grid so direct Title+Description+Action
works.** `Card.css:94-102` + `Card.tsx` JSDoc. The current `flex-direction: row`
arrangement means Title, Description, Action are three columns by
default — the `.zs-card__title + .zs-card__description { margin-block-start }`
rule never fires (the stories work around with an anonymous `<div>` wrapper).
Replace:
```css
.zs-card__header {
  display: grid;
  grid-template-columns: 1fr auto;
  column-gap: var(--zs-space-3);
  align-items: start;
}
.zs-card__title       { grid-column: 1; }
.zs-card__description { grid-column: 1; }
.zs-card__action      { grid-column: 2; grid-row: 1 / -1; align-self: center; }
.zs-card__header > .zs-card__title + .zs-card__description {
  margin-block-start: var(--zs-space-half);
}
```
Update the `Decomposed` story to drop the anonymous wrapper — Title,
Description, and Action become direct children of Header, and the
documented API matches the rendered layout. Add `min-inline-size: 0`
on the title column so long titles shrink (covers #12).

**4. Add a React-19-safe `getElementRef` helper.**
`_slot.ts:105-117` + `Card.tsx:188-193`. React 19 (workspace catalog
pins `react: ^19.2.0`) deprecated `element.ref` for function-component
children — the ref now lives on `element.props.ref`, and `.ref` access
emits a runtime deprecation warning + may return `null`. Refs forwarded
into Card asChild or Card.Title asChild silently stop composing.

Add to `_slot.ts`:
```ts
/**
 * React-version-safe access to a child element's ref. React 19 moved
 * the ref onto `props.ref` for function components; the legacy
 * `element.ref` property emits a deprecation warning and may return
 * null. This helper checks the new location first.
 */
export function getElementRef<T>(element: ReactElement): Ref<T> | undefined {
  const propsRef = (element.props as { ref?: Ref<T> }).ref;
  if (propsRef !== undefined) return propsRef;
  // Fallback for React 18 peer-dep callers.
  return (element as unknown as { ref?: Ref<T> }).ref;
}
```
Update `Slot`:
```ts
const childRef = getElementRef<unknown>(child);
if (slotProps.ref !== undefined || childRef !== undefined) {
  merged.ref = composeRefs(slotProps.ref, childRef);
}
```
And `CardTitle.asChild`:
```ts
ref: composeRefs(ref, getElementRef(child)),
```

**5. `tabIndex={undefined}` overwrites child's explicit tabIndex.**
`_slot.ts:71-96`. `mergeProps` does `{ ...theirs, ...ours }` — when
`ours.tabIndex === undefined`, the spread DOES still overwrite
`theirs.tabIndex` with `undefined`. Fix: filter out undefined values
from `ours` before merging:
```ts
const oursDefined: Record<string, unknown> = {};
for (const [k, v] of Object.entries(ours)) {
  if (v !== undefined) oursDefined[k] = v;
}
const merged: Record<string, unknown> = { ...theirs, ...oursDefined };
```
Apply the same filter to the loop that handles className/style/onXxx
composition so we don't accidentally compose an undefined "ours" over
a defined "theirs".

**6. `asChild` + `<button>` is invalid HTML when Card contains block content.**
`Card.tsx:25-28`. The JSDoc example shows `<Card asChild><a>` — but
mentions `<button>` as a valid render-as target elsewhere. A `<button>`
cannot contain block-level descendants (div, h3, p — which Card.Header,
Card.Title, Card.Description produce). Either rendering breaks
hydration in strict mode OR generates invalid HTML the browser
silently reparses. Fix:
- Update the JSDoc to recommend ONLY anchors (or other inline-block-safe
  elements) for asChild.
- Add a dev-mode warning if the asChild child is a `<button>`: log
  "Card asChild=<button> is invalid HTML because Card subparts render
  block content. Use a real Button outside the card, or wrap the card
  in a hit-area pattern instead."

**7. `Card.Media side="left" | "right"` exposed but not implemented.**
`Card.tsx:55` + `Card.css:166-171`. The CSS only sets negative
`margin-inline-start` / `margin-inline-end` — but the Card root is a
flex COLUMN, so margin-inline doesn't produce a horizontal layout; the
media still stacks vertically with negative side-margins (visually
broken). Decide:
- **(a) [recommended]** Remove `"left"` and `"right"` from the
  `CardMediaSide` union pre-launch. Update the type. Update the brief.
  These values can land in slice 3.5 when we implement a real
  horizontal layout (likely requiring a `Card variant="row"` or a
  grid-based Card root).
- (b) Implement: when `Card.Media side="left"` is present, switch the
  Card root to `display: grid; grid-template-columns: auto 1fr` and
  let media own col 1. Substantial work. Defer.

**8. `Card.Title.asChild` uses hand-rolled `cloneElement`, not Slot.**
`Card.tsx:186-200`. The hand-rolled merge doesn't preserve event-handler
composition (no `defaultPrevented` check), doesn't compose `style`, and
diverges from the documented Slot semantics. Fix: route through Slot
the same way Card root does:
```tsx
if (asChild) {
  if (!isValidElement(children)) {
    if (process.env.NODE_ENV !== "production") {
      console.error("Card.Title asChild expects a single React element; got " + typeof children);
    }
    return null;
  }
  return (
    <Slot
      {...rest}
      ref={ref as Ref<unknown>}
      className={classnames("zs-card__title", className)}
    >
      {children}
    </Slot>
  );
}
```

### 🟡 Surface / API (12)

**9. `asChild` silent null on invalid/multiple children.** `Card.tsx:131`
+ `Card.tsx:187`. Use `Children.only` (which throws in dev with a
helpful message) OR add an explicit `console.error` in dev and return
null in prod:
```tsx
if (!isValidElement(children)) {
  if (process.env.NODE_ENV !== "production") {
    console.error("Card asChild expects a single React element child; received " + typeof children + "; rendering nothing.");
  }
  return null;
}
```

**10. `tabIndex` injection on asChild interactive.** `Card.tsx:138`.
Drop the tabIndex line from the asChild branch entirely — the child
element brings its own focusability semantics (anchors via href,
buttons inherently). If a consumer uses asChild with a non-focusable
target, that's their bug; we shouldn't paper over it.

**11. `:disabled` / `[aria-disabled]` should disable interactive states.**
`Card.css`. Add:
```css
.zs-card[data-interactive]:disabled,
.zs-card[data-interactive][aria-disabled="true"] {
  cursor: not-allowed;
  pointer-events: none;
  opacity: 0.55;
}
```

**12. Header title cell needs `min-inline-size: 0`.** Covered by #3
(part of the Grid rewrite).

**13. Footer doesn't wrap.** `Card.css:213-228`. Add `flex-wrap: wrap;
row-gap: var(--zs-space-2);`. With `justify-content: flex-end` (the
default `end` align), wrapped buttons still trail the footer.

**14. Body should normalize direct-child prose margins.**
`Card.css:206-211`. Add:
```css
.zs-card__body > p:first-child,
.zs-card__body > ul:first-child,
.zs-card__body > ol:first-child { margin-block-start: 0; }
.zs-card__body > p:last-child,
.zs-card__body > ul:last-child,
.zs-card__body > ol:last-child  { margin-block-end: 0; }
```

**15. Media should normalize `img`/`picture`/`video`.**
`Card.css:152`. Add:
```css
.zs-card__media :is(img, picture, video) {
  display: block;
  inline-size: 100%;
  block-size: auto;
  object-fit: cover;
}
```

**16. `Card.Media side="fill"` not aria-hidden by default.**
`Card.tsx:245-256`. When `side="fill"` (a decorative background
layer), default `aria-hidden="true"`:
```tsx
const isDecorative = side === "fill";
return (
  <div
    ref={ref}
    aria-hidden={isDecorative || undefined}
    {...rest}
    className={classnames("zs-card__media", className)}
    data-side={side}
  />
);
```
Consumers who want a meaningful fill-mode media (rare) override via the
spread.

**17. Replace `> *` z-index rule with `isolation: isolate`.**
`Card.css:178-181`. The current `.zs-card > :not(.zs-card__media[data-side="fill"]) { position: relative; z-index: 1; }`
mutates positioning on every direct child — surprising in a render
tree. Replace with a stacking-context boundary on Card root:
```css
.zs-card { isolation: isolate; }
.zs-card__media[data-side="fill"] { z-index: 0; }
/* z-index: 1 on siblings becomes implicit via DOM order in the new stacking context. */
```

**18. Gate `backdrop-filter` to themes that actually use it.**
`Card.css:33-34` + `Dialog.css`. On crystal (opaque `--zs-surface`),
`backdrop-filter` is a no-op visually but still triggers compositing
cost on every Card render. Gate with a sentinel: define
`--zs-material-when-surface-translucent` (the actual filter value) +
`var(--zs-material-regular, none)` indirection so opaque themes get
`none` and translucent themes get the blur. Concrete approach: bind
`--zs-card-backdrop-filter` per theme.
```css
[data-theme="crystal"] { --zs-card-backdrop-filter: none; }
/* future-glass theme would set --zs-card-backdrop-filter: var(--zs-material-regular); */
```
Then in Card.css:
```css
.zs-card {
  backdrop-filter: var(--zs-card-backdrop-filter, var(--zs-material-regular));
  -webkit-backdrop-filter: var(--zs-card-backdrop-filter, var(--zs-material-regular));
}
```
Same treatment on Dialog.Popup.

**19. Card.Footer top spacing / optional divider.**
`Card.css:213`. Add an optional `data-divider="top"` modifier:
```css
.zs-card__footer[data-divider="top"] {
  padding-block-start: var(--zs-space-4);
  border-block-start: 0.0625rem solid var(--zs-separator);
  margin-block-start: var(--zs-space-2);
}
```
Add the `divider?: "top"` prop to CardFooterProps. Default to no
divider; consumers opt-in.

**20. Stories: media-side matrix + real-link Interactive.**
- Add `MediaSides` story showing `side="top"`, `"bottom"`, `"fill"`
  (skip left/right per #7).
- Rewrite `Interactive` story to demonstrate the recommended pattern:
  `<Card asChild interactive><a href="…">…</a></Card>`. The old story
  (interactive plain div without onClick) becomes a CAUTION sub-cell
  documenting the dev warning fires.
- Add `InteractiveWithKeyboard` story per #1 with the aria-wiring check.

### 🟢 Nits (6)

**21. Delete empty `.zs-card--surface { }` rule.** `Card.css:54-56`.

**22. Replace hardcoded `font-weight: 600` with a token.**
`Card.css:109, 114, 119`. Use `var(--zs-text-headline-weight)` which
already resolves to 600. Same swap on `--zs-text-title-2-weight` and
`--zs-text-title-3-weight` if they don't already match. Consolidates
the typography contract.

**23. Export subpart prop interfaces.** `Card/index.ts`. Add
`CardHeaderProps`, `CardBodyProps`, `CardActionProps`, `CardMediaProps`,
`CardFooterProps`, `CardTitleProps`, `CardDescriptionProps` so wrapper
components can typecheck.

**24. Fix story copy typo "Tab tab content sit here."** `Card.stories.tsx`.
Use "Tab content sits here." across all variants.

**25. Consolidate `classnames` helper.** `_slot.ts` + `Card.tsx`.
Move `classnames` to a new `sdks/ui/src/components/_classnames.ts` and
import in both. Update Field/Input/Button/Dialog/AlertDialog to import
from there too. (One canonical helper, never duplicated.)

**26. Dead CSS rule already removed by #3.** No-op; track here for completeness.

## Files to modify

- `sdks/ui/src/components/Card/Card.tsx` — items 1, 6, 7, 8, 9, 10, 16
- `sdks/ui/src/components/Card/Card.css` — items 2, 3, 11, 13, 14, 15, 17, 18, 19, 21, 22
- `sdks/ui/src/components/Card/index.ts` — item 23
- `sdks/ui/src/components/_slot.ts` — items 4, 5
- `sdks/ui/src/components/_classnames.ts` — NEW (item 25)
- `sdks/ui/src/components/Button/Button.tsx` — item 25 (import classnames)
- `sdks/ui/src/components/Field/Field.tsx` — item 25
- `sdks/ui/src/components/Input/Input.tsx` — item 25
- `sdks/ui/src/components/Dialog/Dialog.tsx` — item 25, also #18 (gate backdrop-filter)
- `sdks/ui/src/components/Dialog/Dialog.css` — item 18
- `sdks/ui/src/components/AlertDialog/AlertDialog.tsx` — item 25
- `sdks/ui/src/stories/Card.stories.tsx` — items 3 (drop wrapper), 20, 24
- `sdks/ui/src/styles.css` — crystal theme additions: `--zs-card-hover-tint`,
  `--zs-card-backdrop-filter: none` for crystal (item 2 + 18)
- `sdks/ui/scripts/check-aria-wiring.mjs` — extend for interactive
  keyboard activation (item 1)
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register new story ids
- `sdks/ui/scripts/capture-card-evidence.mjs` — register new story ids

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity (incl. comments):
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty.
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty.
   - `grep -rn 'zs-blur' sdks/ui apps/zeroship-builder` → empty.
4. `pnpm --filter zeroship-builder build` → green.
5. A11y violations: must report `A11y clean for N stories across 1 themes`
   where N = 52 existing + 2-3 new Card stories (MediaSides + revised
   Interactive variants).
6. A11y incomplete sweep: write a temp scanner; 0 background-gradient,
   0 pseudo-element across all stories. The new hover-tint via
   `background-image: linear-gradient` is a gradient on the element,
   but axe walks `background-color` first — opaque base remains
   computable. Re-verify. Delete scanner after.
7. Aria-wiring assertions: extend `check-aria-wiring.mjs` with:
   - Interactive Card with onClick: Tab focuses it, Enter fires onClick.
   - Interactive Card with onClick: Tab focuses it, Space fires onClick.
   - Interactive Card without onClick: dev warning fires (assert via
     console-message capture in playwright).
   - Card asChild ref composition under React 19 (post-getElementRef
     fix): consumer ref + Slot ref both land on the rendered element.
8. Capture screenshots.

## Report (stdout)

End with:
- Files changed/created.
- Per-decision confirmation (one line per item 1–26 with file:line ref).
- Token purity grep results — all three.
- Build status (both packages).
- A11y violations result line.
- A11y incomplete result — explicit zero on gradient/pseudo.
- Aria wiring assertions result.
- Screenshot paths.
- One taste note (if any).
- Explicit: "I did NOT commit, push, or merge."
