# Slice 10 — Popover + Tooltip (floating primitives)

**Worktree** `.worktrees/wave1-slice10` on branch `wave1-slice10` off `builder/ui-design@e17f6655`.

Two foundational floating primitives. Popover = on-click anchored panel; Tooltip = on-hover anchored label.

## Goal

- **Popover** — anchored panel with optional Backdrop, Title, Description, Close, Arrow. Open on click of Trigger.
- **Tooltip** — on-hover label with delay-open and delay-close. Provider wraps the app once to share timers.

## Hard constraints

Standard set. DO NOT commit; orchestrator merges.

## API shape

### Popover
```tsx
export interface PopoverProps extends Omit<BasePopoverRootProps, "render"> { … }

export const Popover = ForwardedPopover as PopoverComponent & {
  Trigger: typeof PopoverTrigger;
  Portal: typeof PopoverPortal;
  Backdrop: typeof PopoverBackdrop;  // optional, default skipped
  Popup: typeof PopoverPopup;
  Title: typeof PopoverTitle;
  Description: typeof PopoverDescription;
  Close: typeof PopoverClose;
  Arrow: typeof PopoverArrow;
};
```

### Tooltip
```tsx
export interface TooltipProps extends Omit<BaseTooltipRootProps, "render"> {
  /** Delay before showing tooltip (ms). Default 600. */
  delay?: number;
  className?: string;
}

export const Tooltip = ForwardedTooltip as TooltipComponent & {
  Provider: typeof TooltipProvider;  // wrap app once
  Trigger: typeof TooltipTrigger;
  Portal: typeof TooltipPortal;
  Popup: typeof TooltipPopup;
  Arrow: typeof TooltipArrow;
};
```

Reuses Dialog's Portal/Popup pattern (Slice 3, commit `3a64a726`) plus Floating UI anchoring.

## Files to create

```
sdks/ui/src/components/Popover/Popover.{tsx,css}, index.ts (~280 lines)
sdks/ui/src/components/Tooltip/Tooltip.{tsx,css}, index.ts (~200 lines)
sdks/ui/src/stories/Popover.stories.tsx (10)
sdks/ui/src/stories/Tooltip.stories.tsx (8)
sdks/ui/scripts/capture-{popover,tooltip}-evidence.mjs (NEW)
```

## Files to modify

Barrel + styles.css + a11y + aria-wiring scripts.

## Story matrix (18 total)

### Popover (10)
1. Basic · 2. WithTitleDescription · 3. WithArrow · 4. WithBackdrop (modal-feel) · 5. WithClose · 6. PlacementSide (top/right/bottom/left) · 7. AlignStart/Center/End · 8. NestedInDialog · 9. Disabled (trigger) · 10. RTL.

### Tooltip (8)
11. Basic (hover-only) · 12. WithDelay (custom delay) · 13. WithArrow · 14. PlacementSide · 15. OnFocusable (keyboard tab opens) · 16. RichContent (multi-line) · 17. Disabled · 18. RTL.

## Aria-wiring (5 new)

1. Popover Trigger click → Popup open + aria-expanded=true.
2. Popover ESC closes + focus restore to trigger.
3. Popover WithArrow → SVG arrow renders at the correct side.
4. Tooltip hover-intent: hover trigger, wait delay+50ms, assert popup visible; mouseout closes.
5. Tooltip keyboard: Tab to trigger → popup visible (aria-describedby on trigger).

## Verification gates

A11y 199 stories (181 + 18 new). Aria-wiring 70 PASS + 2 SKIP + 0 FAIL. 18 PNGs.

## Contingencies (decide inline)

- **Popover Backdrop**: SKIP by default (popover-feel, not modal). Provide as opt-in subpart for Modal-style Popovers.
- **Tooltip Provider**: required at app root for delay sharing. Story file wraps each story's render in `<Tooltip.Provider>` since Storybook doesn't have a global provider.
- **Tooltip on touch**: Base UI handles touch-vs-pointer correctly (touch = tap to show). Don't override.
- **Arrow positioning**: Base UI's Arrow auto-aligns to the side via Floating UI. Use the SVG path `M 0,0 L 8,8 L 16,0 Z` (triangle pointing down at bottom side; rotated by Base UI per side).
- **Popover.Close button**: matches Dialog.Close pattern (asChild via Slot, composedOnClick). Reuse the canonical from Dialog.tsx (commit `3a64a726`).
- **Forced-colors mirror**: every state selector at equal specificity inside @media (forced-colors: active). Slice 5/6/7 lesson.

## Report

End with files changed; per-component API (file:line); token purity; tsc + build status; a11y count; aria-wiring counts; contingencies fired; 18 PNG paths; one taste note; "I did NOT commit, push, or merge."

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice10`. Leave uncommitted.
