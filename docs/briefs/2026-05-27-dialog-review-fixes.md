# Dialog code review fixes — Phase 2.B (no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `0064852d`).

Lands every finding from the dual code review (codex `/tmp/codex-dialog-code-review.stdout` lines 7937-8048 + claude inline report). Codex: 4 🔴 + 8 🟡 + 5 🟢. Claude: 1 🔴 + 4 🟡 + 4 🟢. Merged + deduplicated = 5 🔴 + 11 🟡 + 8 🟢 = 24 items.

## Hard constraints (unchanged)
- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens; no Tailwind / `@apply`.
- No raw hex / no raw px (incl. CSS comments).
- HIG anchor as principle but never in source.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real bugs (5)

**1. `Dialog.Close onClick` must compose with Base UI's close handler.**
`Dialog.tsx:414`. Today the default path is `<Button {...closeProps} {...rest} ref onClick variant intent />`. With `closeProps` spread FIRST and `rest` spread AFTER, a consumer-provided `onClick` in `rest` overwrites `closeProps.onClick` — `<Dialog.Close onClick={save}>Save</Dialog.Close>` runs `save` but **leaves the dialog open**.

Fix: destructure `onClick: callerOnClick` from rest; compose explicitly (mirror AlertDialog.Action's pattern at lines 410-421):
```tsx
const composedOnClick = (event) => {
  callerOnClick?.(event);
  if (!event.defaultPrevented) closeProps.onClick?.(event);
};
return <Button {...closeProps} {...restNoClick} ref={ref} onClick={composedOnClick} variant={variant} intent={intent} />;
```

Add a story `DialogCloseWithSaveOnClick` that calls a side-effect AND closes, plus extend `check-aria-wiring.mjs` to assert: clicking a Dialog.Close with caller-supplied onClick fires BOTH the side effect AND the close.

**2. `nativeButton={false}` mismatched with real `<button>` render.**
`Dialog.tsx:396`. `BaseDialog.Close nativeButton={false}` is unconditional, but the non-asChild branch renders `<Button>` (which renders `<button>`). Base UI's `useButton` emits a dev error for this mismatch AND applies non-native handlers (`role="button"`, keyboard handlers) to a native button.

Fix: use `nativeButton={true}` for the default Button path; switch to `nativeButton={false}` only when `asChild` is set AND the child isn't a `<button>`. Detect via `isValidElement(children) && children.type !== "button"`.

**3. `Dialog.Close asChild` hand-rolled cloneElement — route through Slot.**
`Dialog.tsx:399`. Today: ignores `rest`, overwrites child props with `closeProps`, doesn't compose child `onClick`, doesn't merge `className`/`style`, returns empty fragment on invalid children, reads `child.ref` directly (broken under React 19).

Fix: route through the shared `_slot.ts` machinery (`Slot`, `mergeProps`, `getElementRef`). The Cancel/Close pattern across Dialog + AlertDialog should be uniform. Reference AlertDialog.Action's asChild pattern; Dialog.Close should mirror it.

Add dev console.error for invalid children (`isValidElement` check); align with Card.tsx's asChild dev-warning pattern.

**4. RTL centering broken — `inset-inline-start: 50%` + `translate(-50%)`.**
`Dialog.css:55`. Logical `inline-start` flips to `right` in RTL; the physical `translate(-50%)` still moves left. Net: popup is shifted ~one popup-width off-center in RTL.

Two fixes; pick (a):
- **(a) Use physical `left: 50%`** for the centering anchor when paired with `translateX(-50%)`. RTL handled by the OS-level layout; the popup itself doesn't care about reading direction for its centering math.
- (b) Use a real logical transform: `translate3d(...)` with `--rtl-flip: 1` variable. More complex; probably not needed for Dialog (which doesn't have flippable inner layout for centering).

Add an RTL story (`Dialog inside dir="rtl"`) + capture screenshot proving the popup is centered.

**5. `disablePointerDismissal={!dismissible}` needs explicit test coverage + Base UI version pin.**
`Dialog.tsx:155`. Empirically the current call is correct (Base UI 1.5: `disablePointerDismissal: false` enables outside-click dismissal). But the name semantic is non-obvious and easy to flip if Base UI renames or flips the default.

Fix:
- Add an aria-wiring assertion: with `dismissible={true}` (default), clicking the Backdrop closes the Dialog.
- Add a comment near line 155 citing Base UI 1.5 behavior + the prop semantic, so a future Base UI upgrade prompt-readers spot the inversion risk.

### 🟡 Surface / taste / API (11)

**6. `DialogProps` drops useful Base UI root props.**
`Dialog.tsx:75`. We accept `open / defaultOpen / onOpenChange / modal / dismissible / children` and Omit the rest. Base UI exposes `onOpenChangeComplete`, `actionsRef`, `handle`, `triggerId`, `defaultTriggerId`, and payload-render children.

Fix: derive `DialogProps` from `BaseRootProps` via `Omit<BaseRootProps, 'disablePointerDismissal'>` + add `dismissible`. Forward all Base UI props verbatim.

**7. Unlabeled dialogs fail silently.**
`Dialog.tsx:230`. Dialog.Popup can render with neither Dialog.Title nor `aria-label`/`aria-labelledby`. Base UI auto-wires ARIA when Title exists but doesn't warn when no label is present.

Fix: dev-only assertion in Dialog.Popup that warns if `aria-labelledby` and `aria-label` are both empty AND no `<Dialog.Title>` descendant exists. Use `useEffect` to inspect the rendered element + Children walk.

**8. Footer doesn't wrap.**
`Dialog.css:240`. `.zs-dialog__footer` is `display: flex` with no `flex-wrap`. Long labels / 3+ action buttons overflow.

Fix: `flex-wrap: wrap; row-gap: var(--zs-space-2);`.

**9. Body prose margins unnormalized.**
`Dialog.css:230`. Direct `<p>`, `<ul>`, `<ol>` children keep browser default margins.

Fix: normalize like Card.css did (first/last child margin-block reset).

**10. `tint="none"` invisible-modal-blocker hazard.**
`Dialog.tsx:193`. `tint="none"` renders a full-viewport transparent backdrop. With `modal={true}` it traps interaction behind nothing visible.

Fix: dev warn when `tint="none"` is combined with `modal={true}` (or its absence — modal defaults true). Document on the prop's JSDoc that `none` means "transparent click-blocker" — possibly rename to `tint="invisible"` for clarity. Decide on the rename.

**11. Missing `Viewport` and `createHandle` exports.**
`Dialog.tsx:435`. Base UI ships `Dialog.Viewport` (positioning/scroll boundary) and `Dialog.createHandle` (pairs with Trigger's `handle` prop). We expose neither.

Fix: add `Dialog.Viewport` styled passthrough + re-export `createHandle` from Base UI. If we intentionally don't want them, document the narrowing in Dialog.tsx file header.

**12. Full-size dialogs use `100dvw` + ignore safe-area insets.**
`Dialog.css:97`. `[data-size="full"]` pins to `100dvw` × `100dvh`. Mobile notches / home indicators eat content; some browsers include scrollbar gutter in `100dvw` causing horizontal overflow.

Fix: replace with `inset: 0; inline-size: auto; block-size: auto;` and apply `padding-inline: env(safe-area-inset-left) env(safe-area-inset-right); padding-block: env(safe-area-inset-top) env(safe-area-inset-bottom);` on the inner layout.

**13. Trigger / Backdrop / Popup ref types too narrow.**
`Dialog.tsx:168`. `Dialog.Trigger` is `forwardRef<HTMLButtonElement>` but Base UI's `render` can swap the element. Consumers passing `render={<a ...>}` get misleading ref types.

Fix: use `HTMLElement` for render-capable wrappers. Apply same broader typing to Backdrop, Popup, Close.

**14. `data-size/-tint/-placement` clobberable by consumer spread.**
`Dialog.tsx:213, 240`. `{...rest}` is spread AFTER our internal `data-*` attrs. A consumer passing `data-size="sm"` clobbers the variant. CSS selectors break.

Fix: reverse spread order so internal `data-*` wins. The "rest spread last" doctrine in the file header applies to consumer aria/data-passthroughs, not variant-driven internal styling attributes — call out the exception.

**15. `DialogClose` ref typing should be `HTMLButtonElement`.**
`Dialog.tsx:390`. Currently `forwardRef<HTMLElement>`. Consumers' `useRef<HTMLButtonElement>(null)` won't typecheck. Tighten to `HTMLButtonElement` for the default path; accept the asChild constraint.

**16. Footer doesn't wrap (DUPLICATE of #8).** Removed.

### 🟢 Nits (8)

**17. Export complete subpart prop types.**
`Dialog/index.ts`. Add: `DialogTitleProps`, `DialogDescriptionProps`, `DialogBodyProps`, `DialogFooterProps`.

**18. Hardcoded `font-weight: 600` in Dialog.css.**
`Dialog.css:218`. Use `--zs-text-headline-weight` (resolves to 600) for consistency with Card.

**19. Stale anatomy comment in Dialog.css.**
`Dialog.css:4`. Tree implies Backdrop contains Popup; they're siblings under Portal. Rewrite.

**20. Dead `.zs-dialog-trigger` class hook.**
`Dialog.tsx:171`. Class emitted but has no CSS rule. Either add intentional documentation OR drop.

**21. Stories miss risky paths.**
`Dialog.stories.tsx`. No coverage for: Dialog.Close with onClick, Dialog.Close asChild, RTL, missing title (negative test for the dev warn), long footer labels, modal={false}, tint="none" interaction. Add focused stories for the items addressed in this commit.

**22. `data-size="full"` ignores `placement="top"` silently.**
`Dialog.css:97-114`. Combining `size="full" placement="top"` lets full-size win; consumer silently gets no `top` behavior. Dev-warn or document in the type that placement is ignored when `size === "full"`.

**23. Missing baseline `opacity: 1` on Backdrop / Popup.**
`Dialog.css:35, 51`. Transitions go from `auto → 0 → auto`. Non-portable. Add explicit `opacity: 1` to the base.

**24. Coarse-pointer pseudo uses raw `top: 50%; left: 50%`.**
`Dialog.css:202`. Other parts use logical properties. Switch to `inset-block-start: 50%; inset-inline-start: 50%;` for consistency. (Geometry is identical for centering; just consistency.)

## Files to modify

- `sdks/ui/src/components/Dialog/Dialog.tsx` — items 1, 2, 3, 5, 6, 7, 10, 11, 13, 14, 15, 20, 22
- `sdks/ui/src/components/Dialog/Dialog.css` — items 4, 8, 9, 12, 18, 19, 23, 24
- `sdks/ui/src/components/Dialog/index.ts` — item 17
- `sdks/ui/src/stories/Dialog.stories.tsx` — item 21 (new stories)
- `sdks/ui/scripts/check-aria-wiring.mjs` — items 1, 5 (new assertions)
- `sdks/ui/scripts/capture-dialog-evidence.mjs` — items 21 (capture new stories)

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity x5: 0 hex, 0 px (incl. comments), 0 zs-blur, 0 Card.Body, 0 CardBody.
4. `pnpm --filter zeroship-builder build` → green.
5. **A11y violations**: serve storybook-static, run a11y check. Must show `A11y clean for N stories across 1 themes` where N = 56 existing + new Dialog stories from item 21.
6. **A11y incomplete sweep**: 0 background-gradient + 0 pseudo-element across all stories.
7. **Aria wiring**: extended with:
   - DialogCloseOnClickComposes: clicking a Dialog.Close with caller onClick fires both side-effect AND close.
   - DialogDismissibleTrueClickBackdrop: clicking the Backdrop closes a dismissible Dialog.
   - DialogPopupWithoutTitleWarns: dev-warn fires when Popup renders with no Title and no aria-label.
   Plus existing 13 PASS + 1 SKIP + 0 FAIL.
8. RTL story screenshot: popup visibly centered in `dir="rtl"`.
9. Re-capture Dialog PNGs (8 existing + 3-5 new from item 21).

## Report

End with:
- Files changed.
- Per-item confirmation (1–24) with file:line refs.
- Token purity x5 results.
- Build status.
- A11y violations + incomplete.
- Aria-wiring (including new assertions).
- Contingencies that fired.
- Screenshot paths.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
