# AlertDialog code review fixes (Phase 2.C of the missing-reviews plan)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `3a64a726`, post-Phase-2.B).

Closes the dual code review of AlertDialog — codex (2 🔴 + 5 🟡 + 3 🟢) + claude (0 🔴 + 4 🟡 + 3 🟢), deduped to 13 fix items. Codex transcript at `/tmp/codex-alertdialog-code-review.stdout` lines 5800–6090; claude review captured in tasks output `a8e3ee17ebb285faf.output`.

## Goal

One commit closing the 2 real bugs (Escape routing + Cancel onClick clobber — twin of the Dialog.Close bug we just fixed in Phase 2.B) and the 11 surface/taste items. After this passes, AlertDialog is structurally aligned with Dialog: same composition story for Cancel (asChild → Slot, onClick compose), same `nativeButton`-honesty, same shared helpers.

## Hard constraints (unchanged)

- Pre-launch, no back-compat.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`.
- No raw hex, no raw px (incl. comments).
- HIG anchor as principle but **not in source** (per `ee923f08` HIG-scrub).
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties.
- **DO NOT commit, push, or merge.**

## Fix list

### 🔴 Real bugs (2)

**1. ESC dismisses the AlertDialog Root directly — even with no Cancel; even bypassing a disabled Cancel; even skipping Cancel's onClick side-effects.**

`AlertDialog/AlertDialog.tsx:86` (the `AlertDialogRootImpl`'s `onOpenChange` passthrough).

Codex: "Base UI's alert mode forces `disablePointerDismissal=true` and `role="alertdialog"`, but it still enables `escapeKey` dismissal. This implementation does not intercept `reason === "escape-key"`, does not find/click an enabled `AlertDialog.Cancel`, and does not no-op when no Cancel exists." The current header comment claims "ESC closes the Cancel button if present" — claude correctly flagged that as a documentation lie (no code implements it).

The slice contract: ESC should activate Cancel (running its onClick) if present, or be a no-op if no Cancel exists. Outside-press is already structurally blocked by Base UI omitting `disablePointerDismissal` from `AlertDialogRoot.Props` (verified in `node_modules/.pnpm/@base-ui+react@1.5.0_…/@base-ui/react/alert-dialog/root/AlertDialogRoot.d.ts` — `Omit<DialogRoot.Props, 'modal' | 'disablePointerDismissal' | 'onOpenChange' | 'actionsRef' | 'handle'>`).

Fix:
- Add an internal `AlertDialogContext` exposing a `registerCancel(buttonRef)` / `unregisterCancel(buttonRef)` API. `AlertDialogCancel` registers itself on mount, unregisters on unmount. Multiple Cancels → last-registered wins (an alert with two Cancels is a user error, dev-warn fix is item 6).
- In `AlertDialogRootImpl`, wrap `onOpenChange`:
  ```tsx
  const handleOpenChange = (
    open: boolean,
    eventDetails: BaseChangeEventDetails,
  ) => {
    if (!open && eventDetails.reason === "escape-key") {
      const cancelEl = cancelRefHolder.current?.current;
      if (cancelEl && !cancelEl.disabled) {
        eventDetails.cancel();
        cancelEl.click();        // runs the composed Cancel onClick + close
        return;
      }
      if (!cancelEl) {
        eventDetails.cancel();   // hard non-dismissible — no Cancel = no-op
        return;
      }
    }
    onOpenChange?.(open, eventDetails);
  };
  ```
  (Use the same `BaseChangeEventDetails` alias already at line 49.)
- Update the header comment lines 8–12 to match the new mechanism: "ESC activates the Cancel button if present; no-op if absent."
- Stories: add `EscClosesCancel`, `EscNoOpsWithoutCancel`, `EscIgnoresDisabledCancel`. Add aria-wiring assertions for all three (real-path: press ESC, verify Cancel's onClick fired AND popup closed, OR popup stayed open).

**2. `AlertDialog.Cancel` stops auto-closing as soon as the caller supplies `onClick`.**

`AlertDialog/AlertDialog.tsx:339-350`.

Identical shape to the Dialog.Close bug we just fixed in Phase 2.B item 1. The non-`asChild` Cancel path renders `<Button {...closeProps} {...rest} ...>`. If `rest` carries `onClick`, it overwrites `closeProps.onClick` before Button sees it. `AlertDialogAction` got this right (lines 410-421); `AlertDialogCancel` did not.

Fix — apply the same `composedOnClick` shape:
```tsx
const { onClick: callerOnClick, ...restNoClick } = rest;
const composedOnClick = (event: ReactMouseEvent<HTMLElement>) => {
  callerOnClick?.(event);
  if (!event.defaultPrevented) {
    const closeHandler = (closeProps as { onClick?: typeof composedOnClick }).onClick;
    closeHandler?.(event);
  }
};
// then in the non-asChild branch:
<Button
  {...closeProps}
  {...restNoClick}
  ref={composeRefs(ref, (closeProps as { ref?: Ref<HTMLElement> }).ref)}
  variant={variant}
  onClick={composedOnClick}
>
  {children}
</Button>
```

Add a `CancelWithCleanupOnClick` story + aria-wiring assertion that verifies BOTH the caller's onClick fires AND the popup closes. Mirror the `CloseWithSaveOnClick` assertion shape we shipped in Phase 2.B.

### 🟡 Surface / taste / API (7)

**3. `nativeButton={false}` mismatch with real `<button>` render.** `AlertDialog/AlertDialog.tsx:321` (Cancel) and `:398` (Action).

Same shape as Dialog item 2 in Phase 2.B. Both render-prop closures wrap `Button` (which emits a real `<button>`) but tell Base UI `nativeButton={false}`. Base UI dev-warns when this mismatches AND applies non-native button attributes (`role="button"`, `aria-disabled`) to a real `<button>`, polluting the DOM.

Fix — derive `nativeButton` per render path:
- Default Cancel path → `nativeButton={true}` (Button renders `<button>`).
- Default Action path → `nativeButton={true}`.
- `asChild` Cancel path → derive from the child: `asChildIsNativeButton = isValidElement(children) && children.type === "button"`. (AlertDialog.Action has no asChild today; if it gains one in a future round, apply the same derivation.)

**4. Footer counts React nodes, not actual buttons — and the dev-warn uses a brittle `displayName` lookup.**

`AlertDialog/AlertDialog.tsx:246-251` (`countButtonChildren`) and `:266-275` (multiple-primary dev-warn).

Codex + claude both flagged this. `Children.count` treats `null` as 0, treats a Fragment containing 5 buttons as 1, treats string whitespace inconsistently across React versions. The `(child.type as any)?.displayName === "AlertDialog.Action"` check breaks silently if a consumer wraps the Action in `React.memo` or a thin wrapper component.

Two-part fix:
- Replace `countButtonChildren` with a recursive flattener that descends into Fragments/arrays and ignores `null`/`undefined`/`boolean`/string children. Count only elements whose `type` carries the sentinel from the next bullet.
- Attach a sentinel to both `AlertDialogAction` and `AlertDialogCancel`:
  ```ts
  (AlertDialogAction as unknown as { __zsAlertButton: "action" }).__zsAlertButton = "action";
  (AlertDialogCancel as unknown as { __zsAlertButton: "cancel" }).__zsAlertButton = "cancel";
  ```
  `React.memo()` copies static properties — so even memoized wrappers preserve the sentinel. Walk children once via the flattener and key on `child.type.__zsAlertButton` instead of `displayName`.
- Reuse the normalized list for `data-button-count`, multiple-primary detection, destructive-bottom detection (item 5), and Cancel-presence detection (item 6).

**5. `3+` destructive-at-bottom is documented but not enforced + `data-tone` only emitted on the close-wrapped Action path.**

`AlertDialog/AlertDialog.css:61` + `AlertDialog/AlertDialog.tsx:422` + `AlertDialog/AlertDialog.tsx:386-395`.

Codex caught the unenforced ordering; claude caught the `data-tone` inconsistency. Both feed the same fix:
- Emit `data-tone={tone}` on BOTH the `preventClose=true` branch (line 386-395) AND the close-wrapped branch (already at 422).
- After the normalized-children scan (item 4), if `buttonCount === "3+"` AND a destructive Action is NOT last in source order, dev-warn:
  ```
  [AlertDialog] In a 3+ button alert, the destructive action should be last in source order. Found at index N of M.
  ```
  Reorder is intentionally NOT done (would surprise consumers more than help); the dev-warn is the contract enforcement.
- Stories: add a `ThreeButtonsDestructiveBottom` (correct) + `ThreeButtonsDestructiveMisplaced` (a11y disabled, console-warn-only negative test).

**6. Destructive alerts do not warn for a missing Cancel.** `AlertDialog/AlertDialog.tsx:261-285`.

HIG: destructive actions should include Cancel so people have a clear safe exit. Add to the same dev-warn block (gated on `process.env.NODE_ENV !== "production"`): after the normalized-children scan, if any `Action` has `tone="destructive"` AND no `Cancel` is present, warn:
```
[AlertDialog] Destructive action without a Cancel button. People need a clear safe exit — add <AlertDialog.Cancel> to the footer.
```
Story: `DestructiveWithoutCancelWarns` (a11y disabled, negative test).

**7. `AlertDialog.Cancel asChild` drops props/handlers and accesses `child.ref` directly under React 19.**

`AlertDialog/AlertDialog.tsx:324-336`.

Same shape as Dialog item 3 in Phase 2.B. Route through `Slot` from `_slot.ts` instead of `cloneElement(child, {...closeProps, ref: composeRefs(...)})`. The current code:
- Ignores `rest` props the caller set on `<AlertDialog.Cancel>` (className, disabled, aria-*, analytics handlers).
- Overwrites the child's own event handlers.
- Reads `child.ref` directly — exactly the React 19 ref-access path `_slot.ts` was extracted to avoid (`getElementRef`).

Fix — mirror the Dialog.Close.asChild shape we just shipped:
```tsx
if (asChild) {
  if (!isValidElement(children)) {
    if (process.env.NODE_ENV !== "production") {
      console.error("[AlertDialog.Cancel] asChild requires a single React element child.");
    }
    return <></>;
  }
  const childOnClick = (children.props as { onClick?: typeof composedOnClick }).onClick;
  const slotOnClick = (event: ReactMouseEvent<HTMLElement>) => {
    childOnClick?.(event);
    if (!event.defaultPrevented) {
      callerOnClick?.(event);
      if (!event.defaultPrevented) {
        const closeHandler = (closeProps as { onClick?: typeof slotOnClick }).onClick;
        closeHandler?.(event);
      }
    }
  };
  return (
    <Slot
      {...closeProps}
      {...restNoClick}
      ref={composeRefs(
        ref as Ref<unknown>,
        getElementRef(children),
        (closeProps as { ref?: Ref<unknown> }).ref,
      )}
      onClick={slotOnClick}
    >
      {children}
    </Slot>
  );
}
```
Story + aria-wiring: `CancelAsChild` mirroring `CloseAsChild` — pass a custom button via asChild, verify className compose, ref attach, onClick compose, AND close-on-click.

### 🟢 Nits (4)

**8. Dev-warn spams on every render.** `AlertDialog/AlertDialog.tsx:261-285`.

The multiple-primary warning runs during render with no de-dup. Controlled alerts that re-render on internal state churn (input typing inside the alert, async loading) repeat the warning. Move the entire dev-warn block into a `useEffect` keyed by a normalized footer signature — pseudo-key: `flattenedActions.map(c => `${c.__zsAlertButton}:${c.props.tone ?? "normal"}`).join("|")`. Effect runs once per signature change. Same pattern applies to the destructive-bottom (item 5) and missing-Cancel (item 6) warnings — share the effect.

**9. Public prop-types incomplete.** `AlertDialog/index.ts` + `components/index.ts`.

Codex caught: `AlertDialogTitleProps`, `AlertDialogDescriptionProps`, `AlertDialogBodyProps` are not exported. Define and export them from `AlertDialog.tsx`, then re-export from `AlertDialog/index.ts` and `components/index.ts`. Mirror the Phase 2.B Dialog work (we exported `DialogTitleProps` / `DialogDescriptionProps` / `DialogBodyProps` / `DialogFooterProps` / `DialogViewportProps`).

**10. Hoist `composeBaseClass` to `_classnames.ts`.**

Both `Dialog/Dialog.tsx:140-148` and `AlertDialog/AlertDialog.tsx:68-75` carry byte-identical copies. Move to `_classnames.ts` next to the existing `classnames` helper (slice-3 review-fix item 25 already hoisted `classnames` for the same reason). Re-export from there; delete both inline copies. Update all callers across Dialog.tsx and AlertDialog.tsx to import from `_classnames`. Pre-launch is the right window — no back-compat to preserve.

**11. Empty `@media (forced-colors: active)` block.** `AlertDialog/AlertDialog.css:77-82`.

An empty declaration block with only a comment is functionally dead. Either move the comment next to `.zs-alertdialog-popup` (line ~16) explaining that the popup inherits the Dialog forced-colors rules, or delete the block entirely. Prefer deletion — the inheritance is obvious from the class name composition.

**12. Misleading "explicit assignments at module load" comment.** `AlertDialog/AlertDialog.tsx:431-434`.

Claude flagged: the comment claims the `AlertDialog.Action = AlertDialogAction` namespace assignments (lines 453-463) make the Footer's displayName lookup robust. They don't — the `displayName` is set at line 429, and the namespace assignments are unrelated. With item 4 replacing the displayName lookup with a sentinel anyway, this comment is doubly stale. Delete it.

**13. Redundant `justify-content: stretch` / `align-items: stretch`.** `AlertDialog/AlertDialog.css` `.zs-alertdialog__footer` rule (~line 46-47).

Claude flagged: on a grid container with `1fr` / `1fr 1fr` tracks the tracks already fill the container; both `stretch` declarations are no-ops. Remove. The "Reset Dialog footer's flex justification" comment makes sense as intent but the actual reset isn't needed — grid has its own defaults.

## Files to modify

- `sdks/ui/src/components/AlertDialog/AlertDialog.tsx` — items 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 (delete duplicate), 12
- `sdks/ui/src/components/AlertDialog/AlertDialog.css` — items 11, 13
- `sdks/ui/src/components/AlertDialog/index.ts` — item 9
- `sdks/ui/src/components/index.ts` — item 9
- `sdks/ui/src/components/_classnames.ts` — item 10 (add `composeBaseClass`)
- `sdks/ui/src/components/Dialog/Dialog.tsx` — item 10 (remove duplicate, import from `_classnames`)
- `sdks/ui/src/stories/AlertDialog.stories.tsx` — new stories per items 1, 2, 5, 6, 7
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register new stories (per Phase 2.B convention); negative-test stories (`ThreeButtonsDestructiveMisplaced`, `DestructiveWithoutCancelWarns`) excluded via `a11y: { disable: true }` story-meta
- `sdks/ui/scripts/capture-alertdialog-evidence.mjs` — register new stories for capture
- `sdks/ui/scripts/check-aria-wiring.mjs` — new assertions for items 1, 2, 7

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity x5: 0 hex, 0 px, 0 zs-blur, 0 Card.Body, 0 CardBody (Dialog files already clean post-2.B; the AlertDialog work must not regress).
4. `pnpm --filter zeroship-builder build` → green.
5. **A11y clean across all stories** — baseline was 61 post-2.B. Expect +N new positive stories (registered in `check-storybook-a11y.mjs`), with negative-test stories explicitly excluded. Report final count.
6. **A11y incomplete sweep**: 0 background-gradient + 0 pseudo-element on the new stories.
7. **Aria-wiring**: 18+ PASS + 0 FAIL (was 15 PASS + 2 SKIP + 0 FAIL with static URL; with `STORYBOOK_DEV_URL` set, the 2 SKIPs convert + 3 new AlertDialog assertions land = 20). Static-build path keeps the 2 dev-warn SKIPs.
8. Re-capture all AlertDialog PNGs via `capture-alertdialog-evidence.mjs`. Eyeball:
   - `EscClosesCancel` — Cancel's `data-testid="cancel-clicked-status"` updates after ESC.
   - `EscNoOpsWithoutCancel` — alert stays open after ESC, no error.
   - `CancelWithCleanupOnClick` — cleanup status updates AND popup closes.
   - `CancelAsChild` — custom-styled button keeps consumer className, fires close.
   - `ThreeButtonsDestructiveBottom` — destructive last, no warn.

## Contingencies

- **Item 1 (ESC → Cancel routing)**: if `eventDetails.cancel()` isn't the canonical Base UI 1.5 API for canceling open-change requests, check `node_modules/.pnpm/@base-ui+react@1.5.0_…/@base-ui/react/floating-ui-react/types.js` and surrounding files for the actual cancel mechanism. If the API is different, switch to whatever the installed Base UI version exposes; document the choice in a code comment near the wrapped `onOpenChange`.
- **Item 4 (`__zsAlertButton` sentinel + `React.memo`)**: confirm by writing a story that wraps `<AlertDialog.Action>` in `React.memo(() => <AlertDialog.Action tone="destructive">…</AlertDialog.Action>)` and verifying the destructive-bottom warning still fires. If `memo` swallows the sentinel (it shouldn't — `memo` returns a memo object that copies statics from the inner forwardRef), fall back to walking `child.type?.type?.__zsAlertButton` (memo's inner) as a second-pass check.
- **Item 7 (`Slot` for `Cancel.asChild`)**: if the `Slot` import path or signature differs from the one we used in Dialog.Close.asChild (Phase 2.B item 3), match the Dialog implementation byte-for-byte — same import, same `mergeProps` semantics, same dev-error fallback.

## Report

End with:
- Files changed.
- Per-item confirmation (1–13) with file:line refs.
- Token purity grep results (5 greps).
- Build status (3 builds: ui, storybook, builder).
- A11y violations + incomplete sweep (final story count).
- Aria-wiring result (static, plus dev-URL count if dev server alive).
- Contingencies that fired (especially items 1 and 4).
- Screenshot paths for the new stories.
- One taste note.
- Explicit: "I did NOT commit, push, or merge."
