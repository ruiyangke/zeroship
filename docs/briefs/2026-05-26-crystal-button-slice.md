# Slice 1 — Crystal theme + Button (Apple-HIG rebuild)

**Worktree** `.worktrees/ui-design` @ branch `builder/ui-design` (HEAD `e42f7ffa`).
**Status when this brief is written:** post-cleanup. `sdks/ui/src/` contains only
`styles.css`, `theme.tsx`, `index.ts`, `placeholders.tsx`. No components yet.

## Goal

Land the FIRST slice of the design system: foundation tokens (theme-invariant,
HIG-derived), the `crystal` theme palette (light glass), and the Button
component with HIG's full Style × Role matrix.

**ONE theme. ONE component. Do not drift.**

## Hard constraints

- **Worktree single-writer.** You are the only writer for the duration of this run.
- **Pre-launch, no back-compat.** Replace placeholders cleanly. No deprecated aliases, no migration shims.
- **Plain CSS + `--zs-*` custom-property tokens.** No Tailwind, no styled-components, no `@apply`.
- **No raw hex, no raw px in CSS.** Use `oklch(...)` for color, `rem` for lengths (blur radius too — `blur(1rem)`, never `blur(16px)`).
- **HIG is the design principle.** Themes vary only palette + material defaults. The substrate (type/spacing/radii/motion/focus/hit targets) is theme-invariant.
- **Continuous-corner squircle is not natively expressible on web.** Use `border-radius` at HIG-typical radii (looks ~95% Apple to the eye). No SVG mask hacks.
- **System font stack first.** `-apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter", "Segoe UI", system-ui, sans-serif`.
- **44pt min hit target enforced via `--zs-hit-min`** but small visual buttons can be 32px tall if the hit area extends past visible bounds (desktop ok, document the call).
- **`prefers-reduced-motion`** must be honored — every animated state has a no-op fallback.
- **DO NOT commit. DO NOT push. DO NOT merge.** Leave all changes uncommitted; the pilot reviews and commits.

## HIG citations (the source of every taste call below)

- `developer.apple.com/design/human-interface-guidelines/buttons` — Style × Role × Content matrix. "Always include a press state." "Use style — not size — to visually distinguish the preferred choice." "Configure a button to display an activity indicator when… an action that doesn't instantly complete."
- `developer.apple.com/design/human-interface-guidelines/typography` — System fonts; avoid light weights; respect Dynamic Type (web: rem-based scale).
- `developer.apple.com/design/human-interface-guidelines/materials` — Liquid Glass sparingly; thicker materials = better contrast; thinner = retain context.
- `developer.apple.com/design/human-interface-guidelines/color` — Avoid hard-coding system colors; use semantic naming; test light/dark/contrast.
- `developer.apple.com/design/human-interface-guidelines/motion` — Add purposefully; brief & precise feedback; honor Reduce Motion.

## Files to create

- `sdks/ui/src/components/Button/Button.tsx`
- `sdks/ui/src/components/Button/Button.css`
- `sdks/ui/src/components/Button/index.ts`
- `sdks/ui/src/components/index.ts`
- `sdks/ui/src/stories/Button.stories.tsx`
- `sdks/ui/src/stories/story.css` (paints the crystal mesh under stories)
- `sdks/ui/scripts/capture-button-evidence.mjs` (playwright/chromium screenshot harness)

## Files to modify

- `sdks/ui/src/styles.css` — add the full `:root { ... }` foundation token block AND the `[data-theme="crystal"]` theme block; `@import` Button.css at the top.
- `sdks/ui/src/theme.tsx` — `themes = ["crystal"] as const`, `DEFAULT_THEME = "crystal"`, `themeLabels = { crystal: "Crystal" }`.
- `sdks/ui/src/index.ts` — drop `Button` and `ButtonProps` from the placeholders re-export; re-export them from `./components`.
- `sdks/ui/src/placeholders.tsx` — DELETE the `Button` export (and the `ButtonProps` interface). Keep the other 4 placeholders untouched.
- `sdks/ui/.storybook/preview.ts` — `themes: { Crystal: "crystal" }`, `defaultTheme: "Crystal"`. Also `import "../src/stories/story.css"`.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — `themes = [{ label: "Crystal", value: "crystal" }]`, `stories = ["components-button--all-styles", "components-button--all-sizes", "components-button--all-states", "components-button--destructive"]`.

## Foundation tokens — theme-invariant `:root` block

```css
:root {
  /* Font stack — system-ui first; Inter as the web-friendly fallback. */
  --zs-font-system:
    -apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter",
    "Segoe UI", system-ui, sans-serif;
  --zs-font-mono:
    ui-monospace, "SF Mono", Menlo, Consolas, monospace;

  /* HIG type scale — sizes in rem (rem = 16px root by default; respects user
     font-size preference, which is our Dynamic Type analog). */
  --zs-text-large-title-size: 2.125rem;       /* 34 */
  --zs-text-large-title-line: 2.5rem;          /* 40 */
  --zs-text-large-title-weight: 700;
  --zs-text-large-title-tracking: -0.022em;

  --zs-text-title-1-size: 1.75rem;             /* 28 */
  --zs-text-title-1-line: 2.125rem;            /* 34 */
  --zs-text-title-1-weight: 700;
  --zs-text-title-1-tracking: -0.018em;

  --zs-text-title-2-size: 1.375rem;            /* 22 */
  --zs-text-title-2-line: 1.625rem;            /* 26 */
  --zs-text-title-2-weight: 700;
  --zs-text-title-2-tracking: -0.012em;

  --zs-text-title-3-size: 1.25rem;             /* 20 */
  --zs-text-title-3-line: 1.5rem;              /* 24 */
  --zs-text-title-3-weight: 600;
  --zs-text-title-3-tracking: -0.008em;

  --zs-text-headline-size: 1.0625rem;          /* 17 — semibold body */
  --zs-text-headline-line: 1.375rem;           /* 22 */
  --zs-text-headline-weight: 600;
  --zs-text-headline-tracking: -0.004em;

  --zs-text-body-size: 1.0625rem;              /* 17 — regular body */
  --zs-text-body-line: 1.375rem;               /* 22 */
  --zs-text-body-weight: 400;
  --zs-text-body-tracking: -0.004em;

  --zs-text-callout-size: 1rem;                /* 16 */
  --zs-text-callout-line: 1.3125rem;           /* 21 */
  --zs-text-callout-weight: 400;

  --zs-text-subheadline-size: 0.9375rem;       /* 15 */
  --zs-text-subheadline-line: 1.25rem;         /* 20 */
  --zs-text-subheadline-weight: 400;

  --zs-text-footnote-size: 0.8125rem;          /* 13 */
  --zs-text-footnote-line: 1.125rem;           /* 18 */
  --zs-text-footnote-weight: 400;

  --zs-text-caption-1-size: 0.75rem;           /* 12 */
  --zs-text-caption-1-line: 1rem;              /* 16 */
  --zs-text-caption-1-weight: 400;

  --zs-text-caption-2-size: 0.6875rem;         /* 11 */
  --zs-text-caption-2-line: 0.8125rem;         /* 13 */
  --zs-text-caption-2-weight: 400;

  /* 4pt spacing grid */
  --zs-space-0: 0;
  --zs-space-half: 0.125rem;                   /* 2 */
  --zs-space-1: 0.25rem;                       /* 4 */
  --zs-space-2: 0.5rem;                        /* 8 */
  --zs-space-3: 0.75rem;                       /* 12 */
  --zs-space-4: 1rem;                          /* 16 */
  --zs-space-5: 1.25rem;                       /* 20 */
  --zs-space-6: 1.5rem;                        /* 24 */
  --zs-space-7: 2rem;                          /* 32 */
  --zs-space-8: 2.5rem;                        /* 40 */
  --zs-space-9: 3rem;                          /* 48 */
  --zs-space-10: 4rem;                         /* 64 */

  /* Radii — circular border-radius approximating HIG continuous corners */
  --zs-radius-1: 0.25rem;                      /* 4 — chip */
  --zs-radius-2: 0.375rem;                     /* 6 — small button */
  --zs-radius-3: 0.5rem;                       /* 8 — medium button, input */
  --zs-radius-4: 0.625rem;                     /* 10 — large button */
  --zs-radius-5: 0.75rem;                      /* 12 — card, popover */
  --zs-radius-6: 1rem;                         /* 16 — large card */
  --zs-radius-7: 1.375rem;                     /* 22 — sheet */
  --zs-radius-full: 9999rem;                   /* pill */

  /* Motion — spring for press feedback, ease for color/opacity */
  --zs-motion-spring: cubic-bezier(0.5, 1.25, 0.3, 1);   /* gentle overshoot */
  --zs-motion-ease: cubic-bezier(0.22, 1, 0.36, 1);       /* standard ease-out */
  --zs-motion-fast: 150ms;
  --zs-motion-base: 250ms;
  --zs-motion-slow: 400ms;

  /* Focus ring */
  --zs-focus-ring-width: 0.1875rem;            /* 3 */
  --zs-focus-ring-offset: 0.125rem;            /* 2 */

  /* HIG 44pt minimum tappable */
  --zs-hit-min: 2.75rem;                       /* 44 */
}

@media (prefers-reduced-motion: reduce) {
  :root {
    --zs-motion-fast: 1ms;
    --zs-motion-base: 1ms;
    --zs-motion-slow: 1ms;
    --zs-motion-spring: linear;
    --zs-motion-ease: linear;
  }
}
```

## Crystal theme tokens — `[data-theme="crystal"]`

Cool pastel mesh on near-white. Translucent surfaces. System-blue accent.

```css
[data-theme="crystal"] {
  /* Backdrop — layered radial mesh on near-white base */
  --zs-surface-bg:
    radial-gradient(at 15% 15%, oklch(0.88 0.07 255 / 0.66), transparent 55%),
    radial-gradient(at 85% 20%, oklch(0.9 0.06 195 / 0.56), transparent 55%),
    radial-gradient(at 78% 85%, oklch(0.89 0.07 320 / 0.5), transparent 55%),
    oklch(0.97 0.012 255);

  /* Surfaces — translucent for panels (will be used by Card/Dialog/etc.) */
  --zs-surface: oklch(0.97 0.012 255);
  --zs-surface-raised: oklch(1 0 0 / 0.55);
  --zs-surface-overlay: oklch(1 0 0 / 0.66);
  --zs-surface-sunken: oklch(0.95 0.01 255 / 0.5);

  /* Label hierarchy — HIG label / secondary / tertiary / quaternary */
  --zs-label: oklch(0.25 0.02 275);
  --zs-label-secondary: oklch(0.42 0.02 275);
  --zs-label-tertiary: oklch(0.55 0.02 275);
  --zs-label-quaternary: oklch(0.68 0.02 275);

  /* Fill hierarchy — HIG fill / secondary / tertiary / quaternary
     (used for tinted/gray/plain button backgrounds, gauges, etc.) */
  --zs-fill: oklch(0.5 0.02 275 / 0.18);
  --zs-fill-secondary: oklch(0.5 0.02 275 / 0.13);
  --zs-fill-tertiary: oklch(0.5 0.02 275 / 0.09);
  --zs-fill-quaternary: oklch(0.5 0.02 275 / 0.06);

  /* Separators */
  --zs-separator: oklch(0.5 0.02 275 / 0.18);
  --zs-separator-strong: oklch(0.5 0.02 275 / 0.4);

  /* Accent — HIG system-blue analog (saturated indigo-blue) */
  --zs-accent: oklch(0.55 0.18 285);
  --zs-accent-hover: oklch(0.49 0.19 285);
  --zs-accent-active: oklch(0.45 0.2 285);
  --zs-accent-ink: oklch(0.99 0.005 255);

  /* HIG system colors (semantic) */
  --zs-system-red: oklch(0.6 0.22 25);
  --zs-system-red-hover: oklch(0.54 0.23 25);
  --zs-system-red-active: oklch(0.5 0.24 25);
  --zs-system-green: oklch(0.6 0.16 145);
  --zs-system-yellow: oklch(0.85 0.15 95);
  --zs-system-orange: oklch(0.7 0.2 50);

  /* Focus */
  --zs-focus: oklch(0.55 0.18 285);
  --zs-focus-ring-color: oklch(0.55 0.18 285 / 0.3);

  /* Material — glass */
  --zs-blur: blur(1rem) saturate(1.25);
  --zs-backdrop: oklch(0.5 0.05 275 / 0.3);
}
```

## Button — API

Per HIG, **Style** and **Role** are orthogonal — keep them separate in the API.

```tsx
import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

export interface ButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children"> {
  /**
   * Visual style — HIG button styles.
   * - `filled`: prominent, accent fill, white text. The "primary action" look.
   * - `tinted`: translucent accent-tinted fill, accent text. Secondary action.
   * - `gray`: neutral fill, label text. Tertiary action.
   * - `plain`: no chrome, accent text. Link-style.
   * Default `filled`.
   */
  variant?: "filled" | "tinted" | "gray" | "plain";

  /**
   * Semantic role — HIG button roles.
   * - `normal`: no special meaning.
   * - `primary`: the default action; emits `data-role="primary"` for assistive tech.
   * - `cancel`: cancels the current flow; emits `data-role="cancel"`.
   * - `destructive`: destructive action; OVERRIDES the accent palette with system-red
   *   regardless of variant. Per HIG: never combine `primary` + `destructive`.
   * Default `normal`.
   */
  role?: "normal" | "primary" | "cancel" | "destructive";

  /** Size — small 32px, medium 40px (default), large 48px. */
  size?: "small" | "medium" | "large";

  /** Activity indicator. Per HIG: show this for non-instant actions. */
  loading?: boolean;

  /** Leading element (icon, etc.). */
  startSlot?: ReactNode;

  /** Trailing element. */
  endSlot?: ReactNode;

  /** Label content. */
  children?: ReactNode;
}
```

Implementation notes:
- `forwardRef<HTMLButtonElement, ButtonProps>` — needed for popper/portal users.
- Defaults: `variant="filled"`, `role="normal"`, `size="medium"`.
- `role="destructive"` swaps `--zs-accent` for `--zs-system-red` via a CSS modifier class (`zs-button--destructive`); this works regardless of variant.
- When `loading={true}`: the button is `aria-busy="true"`, `disabled` is forced, label gets `visibility: hidden` (preserves width so the button doesn't jump), and an inline SVG spinner is rendered absolutely centered.
- Inline spinner = `<svg viewBox="0 0 16 16">` with a `<circle>` carrying `stroke-dasharray` animation (1s linear infinite rotation). Honor reduced motion by reducing duration to `0.01ms` so it doesn't spin.

## Button — CSS structure

```css
/* === base === */
.zs-button {
  position: relative;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: var(--zs-space-2);
  border: 0;
  cursor: pointer;
  user-select: none;
  font-family: var(--zs-font-system);
  white-space: nowrap;
  text-decoration: none;
  transition:
    background-color var(--zs-motion-fast) var(--zs-motion-ease),
    color var(--zs-motion-fast) var(--zs-motion-ease),
    transform var(--zs-motion-fast) var(--zs-motion-spring);
}

.zs-button:focus-visible {
  outline: var(--zs-focus-ring-width) solid var(--zs-focus-ring-color);
  outline-offset: var(--zs-focus-ring-offset);
}

.zs-button:active:not(:disabled):not([aria-busy="true"]) {
  transform: scale(0.97);
}

.zs-button:disabled,
.zs-button[aria-busy="true"] {
  opacity: 0.3;
  cursor: not-allowed;
  pointer-events: none;
}

/* === sizes === */
.zs-button--small {
  height: 2rem;                      /* 32 */
  padding-inline: var(--zs-space-3); /* 12 */
  font-size: var(--zs-text-subheadline-size);
  line-height: var(--zs-text-subheadline-line);
  font-weight: 600;
  letter-spacing: -0.002em;
  border-radius: var(--zs-radius-2); /* 6 */
}

.zs-button--medium {
  height: 2.5rem;                    /* 40 */
  padding-inline: var(--zs-space-4); /* 16 */
  font-size: var(--zs-text-body-size);
  line-height: var(--zs-text-body-line);
  font-weight: 600;
  letter-spacing: -0.004em;
  border-radius: var(--zs-radius-3); /* 8 */
}

.zs-button--large {
  height: 3rem;                      /* 48 */
  padding-inline: var(--zs-space-5); /* 20 */
  font-size: var(--zs-text-headline-size);
  line-height: var(--zs-text-headline-line);
  font-weight: 600;
  letter-spacing: -0.004em;
  border-radius: var(--zs-radius-4); /* 10 */
}

/* === variants × accent === */
.zs-button--filled {
  background: var(--zs-accent);
  color: var(--zs-accent-ink);
}
.zs-button--filled:hover:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-accent-hover);
}
.zs-button--filled:active:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-accent-active);
}

.zs-button--tinted {
  background: color-mix(in oklch, var(--zs-accent) 18%, transparent);
  color: var(--zs-accent);
}
.zs-button--tinted:hover:not(:disabled):not([aria-busy="true"]) {
  background: color-mix(in oklch, var(--zs-accent) 28%, transparent);
}
.zs-button--tinted:active:not(:disabled):not([aria-busy="true"]) {
  background: color-mix(in oklch, var(--zs-accent) 36%, transparent);
}

.zs-button--gray {
  background: var(--zs-fill-secondary);
  color: var(--zs-label);
}
.zs-button--gray:hover:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-fill);
}
.zs-button--gray:active:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-fill);
}

.zs-button--plain {
  background: transparent;
  color: var(--zs-accent);
}
.zs-button--plain:hover:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-fill-tertiary);
}
.zs-button--plain:active:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-fill);
}

/* === destructive override (works across all variants) === */
.zs-button--destructive.zs-button--filled {
  background: var(--zs-system-red);
}
.zs-button--destructive.zs-button--filled:hover:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-system-red-hover);
}
.zs-button--destructive.zs-button--filled:active:not(:disabled):not([aria-busy="true"]) {
  background: var(--zs-system-red-active);
}
.zs-button--destructive.zs-button--tinted {
  background: color-mix(in oklch, var(--zs-system-red) 18%, transparent);
  color: var(--zs-system-red);
}
.zs-button--destructive.zs-button--tinted:hover:not(:disabled):not([aria-busy="true"]) {
  background: color-mix(in oklch, var(--zs-system-red) 28%, transparent);
}
.zs-button--destructive.zs-button--plain {
  color: var(--zs-system-red);
}
/* gray + destructive stays gray (semantic only) — HIG doesn't define this combo strongly */

/* === loading === */
.zs-button[aria-busy="true"] .zs-button__label {
  visibility: hidden;
}
.zs-button__spinner {
  position: absolute;
  inset: 0;
  display: grid;
  place-items: center;
  pointer-events: none;
}
.zs-button__spinner svg {
  width: 1rem;
  height: 1rem;
  animation: zs-button-spin 1s linear infinite;
}
@keyframes zs-button-spin {
  to { transform: rotate(360deg); }
}
@media (prefers-reduced-motion: reduce) {
  .zs-button__spinner svg { animation-duration: 0.01ms; }
}
```

## Storybook stories — `Button.stories.tsx`

Title: `Components/Button`.

Stories to export (these are the CSF3 named exports — capitalize hyphens to camelCase for the export names so Storybook URLs match the `kebab-case` story IDs in the a11y check):

- `AllStyles` → URL `components-button--all-styles` — a 4-column row: filled / tinted / gray / plain at medium size.
- `AllSizes` → URL `components-button--all-sizes` — a 3-column row: small / medium / large filled buttons.
- `AllStates` → URL `components-button--all-states` — a grid: default / hover (use `:hover` simulated via CSS hover not needed — just show a row of normal states + one with `disabled` + one with `loading`); include focus-visible by tabbing example.
- `Destructive` → URL `components-button--destructive` — same 4-column row as AllStyles but `role="destructive"`.

Each story shows real Buttons with realistic labels ("Save", "Cancel", "Delete", "Continue").

## Story background — `story.css`

```css
.zs-story-main {
  min-height: 100vh;
  padding: var(--zs-space-7);
  background: var(--zs-surface-bg, var(--zs-surface));
  color: var(--zs-label);
  font-family: var(--zs-font-system);
  display: grid;
  place-items: center;
}
```

## Capture script — `scripts/capture-button-evidence.mjs`

A playwright/chromium harness that serves `storybook-static` on a free port,
visits each of the 4 story URLs at `?id=<id>&globals=theme:Crystal`, sets
`deviceScaleFactor: 2`, and writes PNGs to
`storybook-static/theme-evidence/crystal-button-<story-id>.png`. Must clean
up the server + browser on exit (success and error paths).

## Verification — run before reporting, paste output into your report

1. **Build the package.** `pnpm --filter @zeroship/ui build` → green (ESM + DTS).
2. **Build Storybook.** `pnpm --filter @zeroship/ui build-storybook` → green.
3. **Token purity.**
   - `grep -rnE '#[0-9a-fA-F]{3,8}' sdks/ui/src --include='*.css'` → empty.
   - `grep -rnoE '[0-9]+px' sdks/ui/src --include='*.css'` → empty.
4. **Builder build.** `pnpm --filter zeroship-builder build` → green. Confirms the placeholder→real Button swap didn't break the consumer.
5. **A11y.** Serve `storybook-static` on a free port (`npx http-server storybook-static -p <port> -s`) and run `STORYBOOK_URL=http://localhost:<port> node scripts/check-storybook-a11y.mjs`. Must report `A11y clean for 4 stories across 1 themes`.
6. **Evidence screenshots.** Run `node scripts/capture-button-evidence.mjs`. Verify all 4 PNGs exist under `storybook-static/theme-evidence/`.

## Report (print to stdout)

- **Files changed/created** (full paths).
- **Token-purity grep results** (paste the empty-output confirmation).
- **Build status** for both packages.
- **A11y output line**.
- **Evidence screenshot paths** — all 4.
- **One taste note** describing any judgment call you made (e.g., exact accent hue, motion curve, padding ratio) so the pilot can adjust.
- State clearly that you did NOT commit / push / merge.
