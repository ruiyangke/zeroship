# Card visual polish + shadcn-API improvements (10 items, no deferrals)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `86bc2652`).

Combines two parallel reviews:

1. **Codex visual review** of 12 Card screenshots — `/tmp/codex-card-visual-review.stdout` (full content; the `-o` file was blocked by codex's read-only sandbox). 4 🔴 + 8 🟡 + 6 🟢 findings.

2. **Pilot shadcn API comparison** — `https://github.com/shadcn-ui/ui/blob/main/apps/v4/registry/new-york-v4/ui/card.tsx`. 3 strong improvements + 2 worth-considering + 4 disagree-with-shadcn (we're right).

This brief folds the impactful subset into one polish commit. Pure visual-only complaints about stories/placeholder aesthetics are skipped — they're demo concerns, not component concerns.

## Goal

One commit that closes the 10 items below. Builds + a11y verifier remain green. The visual review's "would not ship" verdict converts to "ready to ship". No new placeholder removal; this is post-Card-shipped polish.

## Hard constraints (unchanged)

- Worktree single-writer.
- Pre-launch, no back-compat. Free to rename / restructure.
- Plain CSS + `--zs-*` tokens. No Tailwind, no `@apply`, no styled-components.
- No raw hex, no raw px (incl. CSS comments).
- HIG anchor; Base UI is the headless layer.
- `prefers-reduced-motion`, `@media (forced-colors: active)`, RTL via logical properties — mandatory.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bugs (4)

**1. Surface variant gets a subtle edge differentiator.**
`Card.css` — `.zs-card--surface` rule. The variant currently has NO boundary mark, so when a surface Card sits on a same-color parent surface, it collapses into text. Add a token-driven subtle inner separator hairline:
```css
.zs-card--surface {
  box-shadow: var(--zs-card-surface-edge, 0 0 0 0.0625rem var(--zs-fill-tertiary) inset);
}
```
Add the token to `:root` (theme-invariant gate) AND to the crystal theme block:
```css
:root { --zs-card-surface-edge: 0 0 0 0.0625rem var(--zs-fill-tertiary) inset; }
[data-theme="crystal"] {
  /* Theme can opt out by setting --zs-card-surface-edge: none if its
     palette already provides sufficient contrast. */
}
```
Verify visually: the all-variants story should now show the surface card as a distinct cell, not text.

**2. Card.Media truly edge-bleeds to the card's border-radius.**
`Card.css` — Media side rules. The current negative-margin pattern (`margin-inline: calc(var(--zs-card-padding-md) * -1)`) should pull Media to the edge, but the screenshot showed it inset. Two likely causes; fix both:

- **Cause A**: Card root's `gap: var(--zs-card-gap-md)` adds vertical space between flex children — when Media is followed by Header, the gap creates a visible gap. Override with adjacency-aware spacing:
  ```css
  .zs-card > .zs-card__media[data-side="top"] + * { margin-block-start: var(--zs-card-padding-md); }
  .zs-card--sm > .zs-card__media[data-side="top"] + * { margin-block-start: var(--zs-card-padding-sm); }
  .zs-card--lg > .zs-card__media[data-side="top"] + * { margin-block-start: var(--zs-card-padding-lg); }
  ```
- **Cause B**: The negative margin must EXCEED the card's own padding to reach the radius — which it does. But if the Card's `border-radius` rounds the corner, the Media also needs `border-top-left-radius` and `border-top-right-radius` inheriting Card's radius for the visual continuity. Add:
  ```css
  .zs-card__media[data-side="top"] {
    border-start-start-radius: inherit;
    border-start-end-radius: inherit;
  }
  .zs-card__media[data-side="bottom"] {
    border-end-start-radius: inherit;
    border-end-end-radius: inherit;
  }
  ```
  This makes the media's rounded corner LITERALLY MATCH the card's rounded corner — the "continuous corner" feel codex flagged as missing.

**3. Ref-composition story layout cleanup.**
`Card.stories.tsx — AsChildRefComposition story`. Codex describes "right-side container looks clipped/half-rendered; button/status pair floats inside an oddly cropped container." Inspect the story; the verification badge/button likely overflows or its layout breaks. Rewrite the story so the verification region is a clean labeled card-shaped subsection without visual ambiguity. Keep the assertion: ref reaches the rendered `<a>`.

**4. Capture hover + focus state screenshots.**
`scripts/capture-card-evidence.mjs`. Extend the capture script to:
- For `components-card--interactive`: simulate `page.hover('.zs-card[data-interactive]')` and capture as `crystal-card-components-card--interactive-hover.png`.
- For `components-card--interactive-with-keyboard`: simulate `page.focus()` via `page.keyboard.press("Tab")` to land on the card, capture as `crystal-card-components-card--interactive-with-keyboard-focus.png`.
- For `components-card--as-child`: hover and capture as `crystal-card-components-card--as-child-hover.png`.

Net: the visual evidence proves the interaction design that static screenshots couldn't.

### 🟡 Taste calls worth fixing (3)

**5. Shift `--zs-accent` hue from 285 (indigo-purple) toward Apple systemBlue (~250).**
`styles.css` crystal palette. Current value:
```
--zs-accent: oklch(0.55 0.18 285);
--zs-accent-hover: oklch(0.49 0.19 285);
--zs-accent-active: oklch(0.45 0.2 285);
```
HIG iOS systemBlue centers around oklch hue 245–255. Shift to:
```
--zs-accent: oklch(0.55 0.18 250);
--zs-accent-hover: oklch(0.49 0.19 250);
--zs-accent-active: oklch(0.45 0.2 250);
```
This affects every component using `--zs-accent` (Button, Input focus ring, etc.) — that's the point; it's a theme-level palette shift. Re-run a11y across all 56 stories afterward and confirm:
- Contrast holds (deeper blue might affect tinted-button contrast on the crystal mesh).
- The visual is HIG-aligned (look at one Button screenshot post-shift; the indigo should now read clearly blue).

If a11y breaks for any combination, deepen the lightness slightly (e.g., 0.55 → 0.5) before declaring failure.

**6. Card.Description uses a more muted ink color.**
`Card.css`. Currently `color: var(--zs-label-secondary)`. Codex notes body text is too close in visual weight to description. Switch to `--zs-label-tertiary` so the description visually recedes further. This is color-based hierarchy, not size — keeps the type scale clean.

```css
.zs-card__description { color: var(--zs-label-tertiary); }
.zs-card[data-disabled] .zs-card__description { color: var(--zs-label-quaternary); }
```

Re-verify a11y — `label-tertiary` (oklch(0.55 0.02 275)) at footnote/subheadline size on `--zs-surface` should still clear WCAG-AA. If borderline, keep on label-secondary and instead bump font-weight down to 400 (already is).

**7. Form-inside Card uses smaller defaults via story conventions.**
NOT a component change — codex notes Input + Button inside Card look chunky. The fix is at the STORY level, not the component:
- Update `Card.stories.tsx WithFormInside` to use `<Input size="sm">` and `<Button size="small">`. Cards-with-forms in real apps will follow this pattern; the story should demonstrate it.
- Document in the story description that forms inside cards typically use smaller controls (a HIG-aligned convention; macOS list-row forms use mini controls).

No component code change required.

### 🟢 Shadcn API improvements (3 strong)

**8. `:has()` Header column collapse — 1-col when no Card.Action present, 2-col when present.**
`Card.css — .zs-card__header`. The current 2-col grid is always reserved; cards without an Action waste the second column. Replace with:
```css
.zs-card__header {
  display: grid;
  grid-template-columns: 1fr;
  column-gap: var(--zs-space-3);
  row-gap: var(--zs-space-half);
  align-items: start;
}
.zs-card__header:has(.zs-card__action) {
  grid-template-columns: 1fr auto;
}
.zs-card__header > .zs-card__title       { grid-column: 1; min-inline-size: 0; }
.zs-card__header > .zs-card__description { grid-column: 1; min-inline-size: 0; }
.zs-card__header > .zs-card__action      { grid-column: 2; grid-row: 1 / -1; align-self: center; }
```
Browser support: `:has()` works on Chrome 105+ / Safari 15.4+ / Firefox 121+. All our targets.

**9. `data-slot="card-{name}"` attribute on every subpart.**
`Card.tsx`. Add `data-slot="card-header"`, `data-slot="card-title"`, `data-slot="card-description"`, `data-slot="card-action"`, `data-slot="card-media"`, `data-slot="card-content"` (post-rename), `data-slot="card-footer"`. The existing className attribute stays — both coexist. This enables a future `[data-slot="card-action"]`-based CSS pattern (shadcn-style), and aligns vocabulary with the shadcn ecosystem.

```tsx
const CardHeader = forwardRef<HTMLDivElement, DivProps>(
  function CardHeader({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        data-slot="card-header"
        className={classnames("zs-card__header", className)}
        {...rest}
      />
    );
  },
);
```
Apply the same pattern to Title, Description, Action, Media, Content (post-rename), Footer. The Card root keeps its existing `data-variant`/`data-size`/`data-interactive` AND gains `data-slot="card"`.

**10. Rename `Card.Body` → `Card.Content`.**
Vocabulary parity with shadcn / MUI / Chakra v3. Pre-launch no-back-compat. Touches:
- `Card.tsx`: `CardBody` → `CardContent`; `Card.Body = …` → `Card.Content = …`; the displayName `"Card.Body"` → `"Card.Content"`.
- `Card.css`: `.zs-card__body` → `.zs-card__content`.
- `Card/index.ts`: re-export rename.
- `Card.stories.tsx`: every `<Card.Body>` → `<Card.Content>` (~6 sites).
- `components/index.ts`: rename the `CardBody` type re-export to `CardContent`.
- `apps/zeroship-builder/**`: grep for `Card.Body` — none expected, but verify.

## Files to modify

- `sdks/ui/src/components/Card/Card.tsx` — items 9 (data-slot), 10 (rename), 3 (story-coupled rename only)
- `sdks/ui/src/components/Card/Card.css` — items 1, 2, 6, 8, 10
- `sdks/ui/src/components/Card/index.ts` — item 10
- `sdks/ui/src/components/index.ts` — item 10
- `sdks/ui/src/stories/Card.stories.tsx` — items 3, 7, 10
- `sdks/ui/src/styles.css` — items 1 (new `--zs-card-surface-edge`), 5 (accent hue shift)
- `sdks/ui/scripts/capture-card-evidence.mjs` — item 4 (hover + focus captures)
- `sdks/ui/scripts/check-storybook-a11y.mjs` — items 4 (new story ids if needed)

## Verification

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. Token purity:
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty.
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty.
   - `grep -rn 'zs-blur' sdks/ui apps/zeroship-builder` → empty.
   - **NEW**: `grep -rn 'Card\.Body' sdks apps` → empty (the rename completed).
   - **NEW**: `grep -rn 'CardBody' sdks apps` → empty.
4. `pnpm --filter zeroship-builder build` → green. The builder doesn't use Card.Body today but verify.
5. **A11y violations**: must report `A11y clean for 56 stories across 1 themes (no serious/critical violations)`. **CRITICAL**: the accent hue shift (item 5) is the highest-risk change for a11y — it touches every Button + Input focus ring + Field error highlight. If contrast drops on tinted Buttons at the new hue, deepen lightness within the brief's permitted bounds OR rollback the shift and document.
6. **A11y incomplete sweep**: 0 background-gradient + 0 pseudo-element across all 56 stories.
7. **Aria-wiring**: `check-aria-wiring.mjs` must still pass 13 + 1 SKIP + 0 FAIL.
8. Capture all Card PNGs (12 existing + 3 new hover/focus) via the updated capture script.

## Report (stdout)

End with:
- Files changed/created.
- Per-decision confirmation (one line per item 1–10 with file:line ref).
- Token purity grep results — all 5 greps.
- Build status (both packages).
- A11y violations result.
- A11y incomplete result — explicit zero on gradient/pseudo.
- Aria wiring assertions result.
- Screenshot paths — all 12+3 = 15 Card PNGs.
- One taste note (if any — especially around the accent hue shift).
- Explicit: "I did NOT commit, push, or merge."
