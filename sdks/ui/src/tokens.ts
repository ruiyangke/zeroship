export const colorTokenNames = [
  "--zs-surface",
  "--zs-surface-raised",
  "--zs-surface-sunken",
  "--zs-surface-overlay",
  "--zs-ink",
  "--zs-ink-soft",
  "--zs-ink-muted",
  "--zs-accent",
  "--zs-accent-hover",
  "--zs-accent-ink",
  "--zs-state-success",
  "--zs-state-success-bg",
  "--zs-state-warn",
  "--zs-state-warn-bg",
  "--zs-state-danger",
  "--zs-state-danger-bg",
  "--zs-state-info",
  "--zs-state-info-bg",
  "--zs-rule",
  "--zs-rule-strong",
  "--zs-focus",
  "--zs-backdrop",
  "--zs-code-bg",
  "--zs-code-ink",
] as const;

export const fontTokenNames = [
  "--zs-font-display",
  "--zs-font-body",
  "--zs-font-mono",
] as const;

export const textTokenNames = [
  "--zs-text-xs",
  "--zs-text-sm",
  "--zs-text-md",
  "--zs-text-lg",
  "--zs-text-xl",
  "--zs-text-2xl",
  "--zs-text-3xl",
  "--zs-line-tight",
  "--zs-line-normal",
  "--zs-line-relaxed",
  "--zs-letter-label",
] as const;

export const spaceTokenNames = [
  "--zs-space-0",
  "--zs-space-1",
  "--zs-space-2",
  "--zs-space-3",
  "--zs-space-4",
  "--zs-space-5",
  "--zs-space-6",
  "--zs-space-8",
  "--zs-space-10",
  "--zs-space-12",
  "--zs-space-16",
] as const;

export const radiusTokenNames = [
  "--zs-radius-xs",
  "--zs-radius-sm",
  "--zs-radius-md",
  "--zs-radius-lg",
  "--zs-radius-xl",
  "--zs-radius-pill",
] as const;

export const shadowTokenNames = [
  "--zs-shadow-xs",
  "--zs-shadow-sm",
  "--zs-shadow-md",
  "--zs-shadow-lg",
  "--zs-shadow-focus",
] as const;

export const motionTokenNames = [
  "--zs-motion-fast",
  "--zs-motion-base",
  "--zs-motion-slow",
  "--zs-motion-ease",
] as const;

export const zTokenNames = [
  "--zs-z-dropdown",
  "--zs-z-popover",
  "--zs-z-modal",
  "--zs-z-toast",
  "--zs-border-sm",
  "--zs-border-md",
] as const;

export const tokenContract = [
  ...colorTokenNames,
  ...fontTokenNames,
  ...textTokenNames,
  ...spaceTokenNames,
  ...radiusTokenNames,
  ...shadowTokenNames,
  ...motionTokenNames,
  ...zTokenNames,
] as const;
