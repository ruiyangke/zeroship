# Fix brief: Feedback blocks (Wave 2a) — merged code-review fixes

Merges codex (CHANGES-NEEDED) + claude (APPROVE-WITH-NITS) reviews. Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Implement + self-verify + report. The
> orchestrator verifies gates and commits.

Severity: 🔴 real bug · 🟡 calibration · 🟢 nit.

---

## 🔴 1. Spinner — consumer `aria-label` / `aria-labelledby` override is ignored

`sdks/ui/src/blocks/Spinner/Spinner.tsx`.

The root unconditionally emits the generated `aria-labelledby={labelId}` BEFORE
`{...rest}`. Per the accessible-name spec, `aria-labelledby` beats `aria-label`,
so a consumer's `<Spinner aria-label="Saving changes" />` is silently ignored —
the announced name stays "Loading". The JSDoc promises the consumer override
wins; it doesn't.

**Fix:** destructure `aria-label` and `aria-labelledby` out of props. Only emit
the generated `aria-labelledby` (and render the internal visually-hidden label
span) when the consumer supplied NEITHER. When the consumer supplied either,
omit the internal label span entirely and let their value provide the name.
Keep the `useId` for the internal path. Order: the consumer's `aria-*` must end
up on the element.

**Regression test (MANDATORY — must fail pre-fix):** add a Spinner story
`<Spinner aria-label="Saving changes" data-testid="spinner-named" />` with a
`play()`:
```ts
expect(canvas.getByRole("status", { name: /saving changes/i })).toBeInTheDocument();
```
Pre-fix the accessible name is "Loading" (aria-labelledby wins) → the query
fails. Post-fix it resolves to "Saving changes" → passes.

## 🟡 2. EmptyState / ErrorState — title-prop vs `.Title`-child precedence is mis-documented

`sdks/ui/src/blocks/EmptyState/EmptyState.tsx` (~L70-73 JSDoc) and
`sdks/ui/src/blocks/ErrorState/ErrorState.tsx` (~L63-65 JSDoc).

The JSDoc claims a `.Title` child "takes precedence / the prop is ignored," but
the render emits the prop-driven title AND `{children}`, so passing both yields
duplicate `<h2>`s. We are NOT adding fragile child-type introspection (the house
Card pattern deliberately avoids it).

**Fix (documentation only):** rewrite the `title` prop JSDoc (and the file-header
usage note) on BOTH components to state the real contract: the component has two
usage modes — (a) ergonomic props (`title`/`description`/`action`/`icon`), or
(b) compound parts (`.Title`/`.Description`/`.Actions`/`.Icon`). Use ONE mode;
when both are supplied the ergonomic content renders first, then children (no
suppression). Remove every "child wins / prop ignored" phrase.

## 🟡 3. Skeleton — multi-line variant drops `width`/`height` on the container

`sdks/ui/src/blocks/Skeleton/Skeleton.tsx` (~L72-93).

In the `lines > 1` branch the container gets only the raw consumer `style`; the
computed `sizeStyle` (folding in `width`/`height`) is unused and `width` lands on
just the first bar.

**Fix:** apply the computed size style to the lines container
(`.zs-skeleton-lines`) so `width`/`height` size the block; let individual bars
size from the container (the last bar stays shorter). Per-line height comes from
the text line-height token — add a one-line JSDoc on `height` clarifying it sizes
the block/container (text-line height derives from the type token).

## 🟢 4. Dead `data-slot="*-column"` on the inner Stack

`ErrorState.tsx` (~L142) and `EmptyState.tsx` (~L125).

`<Center asChild><Stack data-slot="error-state-column" …>` — `Center`'s Slot
merges `data-slot="center"` and then `Stack` writes `data-slot="stack"` after
`{...rest}`, so the intended `*-column` slot never reaches the DOM (dead).

**Fix:** remove the unused `data-slot="*-column"` prop from the inner `Stack` in
both components (the `zs-*-state__column` class still styles it). Don't fight the
primitives' own `data-slot`.

## Deliberate NON-change (documented, per orchestrator)

Codex 🟡: reduced-motion stories assert paint/presence, not computed
`animation:none`. Per-story `prefers-reduced-motion` emulation is a cross-cutting
test-runner-infra change (the existing Progress/Meter precedent asserts paint;
emulation lives in `check-aria-wiring.mjs`, not test-storybook). OUT OF SCOPE for
this slice — do NOT add per-story media emulation. The orchestrator separately
verifies the `@media (prefers-reduced-motion: reduce) { … animation: none }`
rule exists in Skeleton.css + Spinner.css by reading the CSS. Leave the
reduced-motion stories as-is.

---

## Verification (run and REPORT; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src
grep -rn '#[0-9a-fA-F]\{3,8\}\b' --include='*.css' --include='*.tsx' --include='*.ts' blocks/   # 0
grep -rn '[0-9]\+px' --include='*.css' --include='*.tsx' --include='*.ts' blocks/                # 0
# 4 suites on a FREE port:
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6151 --silent &) ; sleep 3
for s in EmptyState ErrorState Skeleton Spinner; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6151 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: the Spinner aria-* diff + the new aria-label regression story, the two
JSDoc rewrites (confirm no "child wins" phrasing remains), the Skeleton
container size fix, the removed dead data-slots, build results, the two grep
counts, and the 4 suites' pass counts.
