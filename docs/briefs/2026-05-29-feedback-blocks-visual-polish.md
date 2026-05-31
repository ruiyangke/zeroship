# Visual-polish brief: Feedback blocks (Wave 2a)

From the codex visual review (APPROVE-WITH-NITS) of the captured PNGs. CSS/token
calibration only — no API changes, no new props. Worktree:
`/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.

> **DO NOT commit, push, or merge.** Apply + self-verify + report.

All 🟡/🟢. Adjust within the `--zs-*` token system only (no raw hex/px).

## In-scope (apply)

1. **🟡 Description type hierarchy** — `EmptyState.css` + `ErrorState.css`.
   Descriptions read a touch too large/dark for secondary copy; the title
   should own the hierarchy. Drop the description to a smaller type token
   (e.g. `--zs-text-subheadline-*` → `--zs-text-footnote-*` if currently
   larger) and/or soften its color one step (`--zs-label-secondary` →
   `--zs-label-tertiary`). Keep titles as-is.

2. **🟢 ErrorState alert-icon weight** — `ErrorState.css` (+ the inline SVG in
   `ErrorState.tsx` if stroke is set there). The built-in alert glyph is a bit
   heavy vs the type. Reduce the icon size token slightly and/or thin the SVG
   `stroke-width` (it's in viewBox units, allowed). (EmptyState's icon is
   consumer-supplied via the story — do NOT change the component for that;
   leave EmptyState's icon alone.)

3. **🟡 Skeleton fill + paragraph rhythm** — `Skeleton.css`. The fill is a bit
   high-contrast/cold; lighten the base one step within the fill family (e.g.
   `--zs-fill-secondary` → `--zs-fill-tertiary`, with the shimmer highlight
   adjusted to stay subtle). Make the multi-line LAST bar a little shorter
   (≈50% instead of 60%) for a more natural paragraph end.

4. **🟡 Spinner `sm` arc** — `Spinner.css`. The `sm` arc looks too thin/short
   vs `md`/`lg`. Normalize the ring stroke so the arc proportion is consistent
   across sizes (scale border-width with size rather than a fixed width), and
   bump the `sm` track contrast slightly so the ring doesn't disappear.

5. **🟡 ErrorState Retry button look** — `ErrorState.tsx`. The Retry currently
   renders `<Button variant="tinted">`, which shows a bordered/double-outline
   look that reads as over-styled at rest. Switch to a cleaner variant — use
   the same variant EmptyState's primary action uses (check Button's variant
   union; prefer a solid/filled primary or a plain bordered "secondary", NOT
   tinted). Reserve the strong ring for keyboard focus (that's Button's
   `:focus-visible`, already correct). One-line change.

## Out of scope (do NOT touch — note only)

- "EmptyState action button oversized / pale-filled reads disabled" — that is
  the shipped `Button` component's sizing/variant, not a block concern. Leave
  `Button` alone. (If trivial, the EmptyState/ErrorState demo STORIES may pass
  an appropriate button `size`/`variant`, but do not modify `Button` itself.)
- Skeleton card-radius/circle-spacing 🟢 — optional; skip unless trivial.

## Verify (run, report; do not commit)

```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build
pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/|wc -l)"
# 4 suites on a FREE port must still pass:
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6155 --silent &) ; sleep 3
for s in EmptyState ErrorState Skeleton Spinner; do npx test-storybook --config-dir .storybook --url http://127.0.0.1:6155 --maxWorkers=1 $s.stories 2>&1 | grep -E 'Tests:|✕'; done
```

Report: which token/value you changed per item (old → new), build results, the
hex/px counts (still 0/0), and the 4-suite pass counts (all must still pass).
