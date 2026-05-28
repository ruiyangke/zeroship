# Slice 5 visual polish — Toggle + Toggle.Group

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (post Slice-5 review fix).

Closes codex visual review: 1 🔴 + 3 🟡 + 1 🟢. Codex transcript at `/tmp/claude-1000/.../tasks/bq0pwzvxk.output:5648-5773`.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / `oklch()` in component CSS.
- Glass-surface invariant + forced-colors mappings.
- DO NOT commit, push, or merge.

## Fix list

### 🔴 Real visual bug (1)

**1. Pressed variants collapse into one visual state.**
`Toggle.css:212-224` (`.zs-toggle[data-pressed]`).

All three variants (default / plain / tinted) paint the same solid accent fill when pressed. The variant ladder — quiet to loud — disappears at the most important state. Default should be a lit-up rectangle; plain should stay ghost-like; tinted should be the loudest accent treatment.

Fix — per-variant pressed tokens:
```css
.zs-toggle {
  --zs-toggle-pressed-bg: var(--zs-accent);
  --zs-toggle-pressed-ink: var(--zs-accent-ink);
  --zs-toggle-plain-pressed-bg: color-mix(in oklch, var(--zs-accent) 14%, transparent);
  --zs-toggle-plain-pressed-ink: var(--zs-accent-active);
  --zs-toggle-tinted-pressed-bg: var(--zs-accent-hover);
  --zs-toggle-tinted-pressed-edge: var(--zs-accent-active);
}

.zs-toggle[data-pressed] {
  background-color: var(--zs-toggle-pressed-bg);
  color: var(--zs-toggle-pressed-ink);
}
.zs-toggle--plain[data-pressed] {
  background-color: var(--zs-toggle-plain-pressed-bg);
  color: var(--zs-toggle-plain-pressed-ink);
}
.zs-toggle--tinted[data-pressed] {
  background-color: var(--zs-toggle-tinted-pressed-bg);
  color: var(--zs-accent-ink);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-toggle-tinted-pressed-edge);
}
```

Mirror in the forced-colors block: plain-pressed → `Highlight`/`HighlightText` (still); tinted-pressed → same (system-defined contrast survives).

### 🟡 Calibration (3)

**2. Icon-only toggles padded like text buttons.**
`Toggle.css:103-105`.

In `WithIconOnly`, `MultipleMode`, and `EqualWidthOff` the glyph buttons feel chunky. Icon-only should be square: width == height.

```css
.zs-toggle:has(> svg:only-child) {
  inline-size: var(--zs-toggle-h-md);
  padding-inline: var(--zs-space-0);
}
.zs-toggle--sm:has(> svg:only-child) { inline-size: var(--zs-toggle-h-sm); }
.zs-toggle--lg:has(> svg:only-child) { inline-size: var(--zs-toggle-h-lg); }
```

**3. Icon+text gap wider than Button equivalent.**
`Toggle.css:91-93`.

`B Bold` reads mechanically separated. Tighten the icon+text gap to match Button.

```css
.zs-toggle:has(> svg) {
  gap: var(--zs-space-1);
}
```

Scoped via `:has(> svg)` so text-only segments keep the roomier rhythm where it serves.

**4. Disabled+pressed slightly too saturated.**
`Toggle.css:253-258` (`.zs-toggle[data-disabled][data-pressed]`).

Lavender fill carries enough accent energy to read close to a tinted enabled control. Drop saturation; carry "was selected" history via a subtle inset edge instead.

```css
.zs-toggle[data-disabled][data-pressed] {
  --zs-toggle-disabled-pressed-bg: color-mix(in oklch, var(--zs-accent) 22%, var(--zs-fill-secondary));
  --zs-toggle-disabled-pressed-edge: color-mix(in oklch, var(--zs-accent) 30%, var(--zs-separator));
  background-color: var(--zs-toggle-disabled-pressed-bg);
  color: var(--zs-label-tertiary);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-toggle-disabled-pressed-edge);
}
```

### 🟢 Nit (1)

**5. Large grouped pressed pill corners feel tab-like.**
`Toggle.css:336-343`.

Drop the radius subtraction at `lg`; the rail inset from group padding already nests the pill.

```css
.zs-toggle-group .zs-toggle[data-size="lg"] {
  border-radius: var(--zs-toggle-radius-lg);
}
```

## Files to modify

- `sdks/ui/src/components/Toggle/Toggle.css` — items 1–5.

(No story changes; no aria-wiring changes — visual-only.)

## Verification gates

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm --filter @zeroship/ui build-storybook` → green.
3. `pnpm --filter zeroship-builder build` → green.
4. Token purity x5 + raw `oklch(` in component CSS = 0.
5. A11y clean for 118 stories (unchanged).
6. Aria-wiring 38 PASS + 2 SKIP + 0 FAIL (unchanged).
7. Re-capture 18 Toggle PNGs. Eyeball the AllVariants triptych — default vs plain vs tinted pressed should now read as three distinct treatments, not one.

## Contingencies (decide inline)

- **Item 1 plain-pressed contrast under axe**: `color-mix(--zs-accent 14%, transparent)` is the same recipe Slice 4 readonly uses; should pass AA against `--zs-accent-active` ink. If axe flags, deepen to 18% (matches the tinted-variant rest tone bumped during Slice 5 implementation). Document inline.
- **Item 2 `:has(> svg:only-child)`**: browser support is Chrome 105+ / Safari 15.4+ / Firefox 121+. All modern, but the rule degrades to text-button padding on older browsers — acceptable fallback. Don't ship a polyfill.
- **Item 4 disabled+pressed edge visibility**: `--zs-selection-hairline` is 0.0625rem (1px). On Crystal's light surface the 30% accent + separator mix should produce a soft visible edge; verify in capture.

## Report

End with files changed; per-item confirmation (1–5) with file:line refs; token purity; build status; a11y count; aria-wiring counts; contingencies fired; 18 screenshot paths; one taste note; "I did NOT commit, push, or merge."
