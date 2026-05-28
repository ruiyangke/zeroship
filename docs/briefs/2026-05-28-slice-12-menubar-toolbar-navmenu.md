# Slice 12 — Menubar + Toolbar + NavigationMenu

**Worktree** `.worktrees/wave2-slice12` on branch `wave2-slice12` off `builder/ui-design@6266a076`.

App chrome surfaces. Menubar = macOS-style menu strip. Toolbar = button row with separators. NavigationMenu = topnav with mega-menu popouts.

## Goal

- **Menubar** — horizontal strip of Menu triggers (File/Edit/View/etc.). Hovering one open menu auto-opens the next on hover.
- **Toolbar** — `role="toolbar"` row with Buttons, ToggleGroup, Separator. Reuses Slice 5 Toggle.Group.
- **NavigationMenu** — multi-level topnav. Each top-level Item can open a wide Content panel below.

## Hard constraints

Standard set. DO NOT commit; orchestrator merges.

## API shape

### Menubar
```tsx
export interface MenubarProps extends Omit<BaseMenubarProps, "render"> { … }
export const Menubar = ForwardedMenubar; // wraps Menu siblings
```

### Toolbar
```tsx
export interface ToolbarProps extends Omit<HTMLAttributes<HTMLDivElement>, "role"> {
  /** Always renders role="toolbar". */
  orientation?: "horizontal" | "vertical";
  className?: string;
}
export const Toolbar = ForwardedToolbar as ToolbarComponent & {
  Separator: typeof ToolbarSeparator;
};
```

(Note: Base UI has no dedicated `toolbar` package in 1.5.0 — verify; if absent, we own role="toolbar" + composition.)

### NavigationMenu
```tsx
export const NavigationMenu = ForwardedNavMenu as NavMenuComponent & {
  List: typeof NavMenuList;
  Item: typeof NavMenuItem;
  Trigger: typeof NavMenuTrigger;
  Content: typeof NavMenuContent;
  Link: typeof NavMenuLink;
  Portal: typeof NavMenuPortal;
  Positioner: typeof NavMenuPositioner;
  Viewport: typeof NavMenuViewport;
  Arrow: typeof NavMenuArrow;
  Icon: typeof NavMenuIcon;
};
```

## Files to create

```
sdks/ui/src/components/Menubar/Menubar.{tsx,css}, index.ts (~180 lines)
sdks/ui/src/components/Toolbar/Toolbar.{tsx,css}, index.ts (~180 lines)
sdks/ui/src/components/NavigationMenu/NavigationMenu.{tsx,css}, index.ts (~320 lines)
sdks/ui/src/stories/{Menubar,Toolbar,NavigationMenu}.stories.tsx (8 + 8 + 8 = 24 stories)
sdks/ui/scripts/capture-{menubar,toolbar,navmenu}-evidence.mjs
```

## Files to modify

Barrel + styles.css + a11y + aria-wiring scripts.

## Story matrix (24 total)

### Menubar (8)
1. Basic File/Edit/View · 2. WithSubmenus · 3. WithCheckboxItem · 4. WithRadioGroup · 5. KeyboardNav (Alt+F, arrow keys) · 6. Disabled · 7. WithIcons · 8. RTL.

### Toolbar (8)
9. Basic button row · 10. WithSeparator · 11. WithToggleGroup (segmented control inline) · 12. WithIconButtons · 13. Vertical · 14. WithGroups (logical clustering via Separator) · 15. Disabled · 16. RTL.

### NavigationMenu (8)
17. Basic · 18. WithContent (mega-menu) · 19. WithIcons · 20. WithViewport (animated content transitions) · 21. WithArrow · 22. KeyboardNav (Tab + arrow keys) · 23. Disabled · 24. RTL.

## Aria-wiring (5 new)

1. Menubar hover one trigger then hover next → second menu auto-opens.
2. Menubar ArrowRight at last trigger loops to first (keyboard roving).
3. Toolbar role="toolbar" + aria-orientation reflects orientation prop.
4. NavigationMenu Trigger click → Content panel opens with aria-expanded=true.
5. NavigationMenu Tab cycles between Items.

## Verification gates

A11y 233+ stories (215 + 24 — depends on slice 11 landing first; orchestrator handles merge). Aria-wiring 80 PASS + 2 SKIP + 0 FAIL. 24 PNGs.

## Contingencies (decide inline)

- **Toolbar without Base UI primitive**: if `@base-ui/react/toolbar` doesn't exist, build from native + role="toolbar". Compose Buttons / Toggle.Group / Separator. Verify keyboard roving via Tab → focus to first; arrow keys roving inside.
- **Menubar auto-open-on-hover-after-first-click**: Base UI handles this. Don't override.
- **NavigationMenu Content sizing**: max-width fits the viewport; Positioner offsets handle alignment.
- **Forced-colors mirror**: standard.
- **Hit-target ≥ 1.75rem on items**.

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave2-slice12`. Leave uncommitted.

## Report

Standard report format.
