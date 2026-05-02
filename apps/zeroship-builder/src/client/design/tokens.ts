// CSS-level tokens are the source of truth (index.css). This file re-exports
// them as TypeScript constants for places where we need raw values in JS
// (canvas drawing, third-party lib config, etc.). Keep in sync with index.css.

export const colors = {
  paper:    "oklch(0.97 0.012 89)",
  paper2:   "oklch(0.94 0.014 86)",
  paper3:   "oklch(0.91 0.014 84)",
  ink:      "oklch(0.18 0.013 60)",
  inkSoft:  "oklch(0.40 0.013 65)",
  pencil:   "oklch(0.55 0.012 70)",
  rule:     "oklch(0.78 0.014 75)",
  rule2:    "oklch(0.85 0.014 78)",
  tomato:   "oklch(0.61 0.21 27)",
  tomato2:  "oklch(0.55 0.21 25)",
  tomato3:  "oklch(0.90 0.06 27)",
  ivy:      "oklch(0.58 0.16 152)",
  ivy2:     "oklch(0.50 0.16 150)",
  ivy3:     "oklch(0.93 0.05 150)",
  cobalt:   "oklch(0.55 0.18 252)",
  amber:    "oklch(0.72 0.15 80)",
  blood:    "oklch(0.50 0.21 28)",
} as const;

export const fonts = {
  display: '"Fraunces", "Times New Roman", serif',
  serif:   '"Source Serif 4", Georgia, serif',
  sans:    '"Inter", system-ui, sans-serif',
  mono:    '"JetBrains Mono", ui-monospace, monospace',
} as const;

export const motion = {
  ease: "cubic-bezier(.2, .7, .2, 1)",
  durations: { fast: 120, base: 200, slow: 380 },
} as const;
