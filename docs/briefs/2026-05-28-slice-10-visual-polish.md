# Slice 10 visual polish — Popover + Tooltip

**Worktree** `.worktrees/ui-design` directly (Slice 10 already merged at HEAD `495d327a` then forward).

Codex visual review: 1🔴 + 4🟡 + 1🟢. Transcript at `/tmp/.../b3fyrk7at.output:2887-3041`.

## Hard constraints

Standard set. DO NOT commit, push, or merge.

## Fix list

### 🔴 (1)

**1. Popover arrow intrudes into readable content** — `Popover.css:98, 163, 169`.

Arrow visually sits inside popup, colliding with the title text. Fix: `z-index: -1` on the arrow so it sits behind content; add drop-shadow:

```css
.zs-popover-popup {
  overflow: visible;
}
.zs-popover-arrow {
  z-index: -1;
  color: var(--zs-surface);
  filter: drop-shadow(
    0 var(--zs-selection-hairline) var(--zs-space-half)
      color-mix(in oklch, var(--zs-label) 24%, transparent)
  );
}
```

### 🟡 (4)

**2. Popover elevation too dialog-like** — `Popover.css:86`, `styles.css:215`.

Reuses `--zs-shadow-dialog`. Replace with new `--zs-shadow-popover` foundation token:

```css
:root {
  --zs-shadow-popover:
    0 var(--zs-space-4) var(--zs-space-8) color-mix(in oklch, var(--zs-label) 16%, transparent),
    0 var(--zs-space-1) var(--zs-space-4) color-mix(in oklch, var(--zs-label) 10%, transparent),
    0 0 0 var(--zs-selection-hairline) color-mix(in oklch, var(--zs-label) 10%, transparent);
}
.zs-popover-popup {
  box-shadow: var(--zs-shadow-popover), inset 0 0 0 var(--zs-selection-hairline) var(--zs-separator);
}
```

**3. Bare Popover oversized for helper copy** — `Popover.css:75, 78, 80`.

Smaller default — callout type, narrower min, tighter padding:

```css
.zs-popover-popup {
  font-size: var(--zs-text-callout-size);
  line-height: var(--zs-text-callout-line);
  min-inline-size: 12rem;
  max-inline-size: min(22rem, calc(100dvw - var(--zs-space-6) * 2));
  padding-block: var(--zs-space-3);
  padding-inline: var(--zs-space-3);
}
.zs-popover__title {
  font-size: var(--zs-text-headline-size);
  line-height: var(--zs-text-headline-line);
}
```

**4. Popover.Close full-width gray bar** — `Popover.css:82`, `Popover.tsx:367`.

Flex column stretch makes Close into a full-width slab. Right-align it:

```css
.zs-popover__close {
  align-self: flex-end;
  min-inline-size: 6rem;
  margin-block-start: var(--zs-space-1);
}
```

Add `className="zs-popover__close"` to the rendered Button in Popover.Close.

**5. Tooltip arrow too subtle** — `Tooltip.css:108, 112`, `Tooltip.tsx:267`.

Arrow size 0.75rem × 0.375rem disappears against dark chip. Bump to 1rem × 0.5rem + drop-shadow:

```css
.zs-tooltip-arrow {
  inline-size: 1rem;
  block-size: 0.5rem;
  color: var(--zs-label);
  filter: drop-shadow(
    0 var(--zs-selection-hairline) var(--zs-space-half)
      color-mix(in oklch, var(--zs-label) 24%, transparent)
  );
}
```

### 🟢 (1)

**6. Tooltip rich-label shadow soften** — `Tooltip.css:45, 56, 58`.

Drop from 32% to 22%:

```css
.zs-tooltip-popup {
  box-shadow:
    0 var(--zs-space-1) var(--zs-space-3)
      color-mix(in oklch, var(--zs-label) 22%, transparent),
    0 0 0 var(--zs-selection-hairline)
      color-mix(in oklch, var(--zs-label) 10%, transparent);
}
```

## Files to modify

- `sdks/ui/src/styles.css` — item 2 (new `--zs-shadow-popover` foundation token).
- `sdks/ui/src/components/Popover/Popover.css` — items 1, 2, 3, 4.
- `sdks/ui/src/components/Popover/Popover.tsx` — item 4 (add className to Close's Button).
- `sdks/ui/src/components/Tooltip/Tooltip.css` — items 5, 6.

## Verification

Builds + token purity + a11y + aria-wiring all unchanged. Re-capture 17 PNGs (9 Popover + 8 Tooltip) and verify the WithArrow Popover shows arrow as edge pointer (not inside content), WithClose's "Got it" right-aligned, Tooltip WithArrow arrow visible.

## Report

End with files changed; per-item confirmation; builds; PNG paths; one taste note; "I did NOT commit, push, or merge."
