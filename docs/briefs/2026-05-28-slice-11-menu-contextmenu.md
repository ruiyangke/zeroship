# Slice 11 — Menu + ContextMenu

**Worktree** `.worktrees/wave2-slice11` on branch `wave2-slice11` off `builder/ui-design@6266a076`.

Two menu surfaces from Base UI: `menu` (click-trigger dropdown menu) and `context-menu` (right-click). Both share Item / Group / Separator / CheckboxItem / RadioGroup subparts.

## Goal

- **Menu** — anchored menu (click trigger). Subparts: Item, Group, GroupLabel, Separator, CheckboxItem + Indicator, RadioGroup + RadioItem + Indicator, LinkItem, SubmenuRoot + SubmenuTrigger, Arrow.
- **ContextMenu** — right-click anchored variant. Internally renders a Menu; trigger handles `contextmenu` event.

## Hard constraints

Standard set. DO NOT commit; orchestrator merges.

## API shape

```tsx
export const Menu = ForwardedMenu as MenuComponent & {
  Trigger: typeof MenuTrigger;
  Portal: typeof MenuPortal;
  Backdrop: typeof MenuBackdrop;       // opt-in
  Popup: typeof MenuPopup;
  Item: typeof MenuItem;
  Group: typeof MenuGroup;
  GroupLabel: typeof MenuGroupLabel;
  Separator: typeof MenuSeparator;
  CheckboxItem: typeof MenuCheckboxItem;
  RadioGroup: typeof MenuRadioGroup;
  RadioItem: typeof MenuRadioItem;
  LinkItem: typeof MenuLinkItem;       // anchor wrapper
  Submenu: typeof MenuSubmenu;         // SubmenuRoot + SubmenuTrigger composed
  Arrow: typeof MenuArrow;
};

export const ContextMenu = ForwardedContextMenu as ContextMenuComponent & {
  Trigger: typeof ContextMenuTrigger;  // wraps anchor area
  /* shares all the Menu subparts via re-export */
};
```

## Files to create

```
sdks/ui/src/components/Menu/Menu.{tsx,css}, index.ts (~450 lines — many subparts)
sdks/ui/src/components/ContextMenu/ContextMenu.{tsx,css}, index.ts (~150 lines — thin trigger over Menu)
sdks/ui/src/stories/Menu.stories.tsx (12 stories)
sdks/ui/src/stories/ContextMenu.stories.tsx (6 stories)
sdks/ui/scripts/capture-{menu,contextmenu}-evidence.mjs
```

## Files to modify

Barrel + styles.css + a11y + aria-wiring scripts.

## Story matrix (18 total)

### Menu (12)
1. Basic items · 2. WithGroups (+ GroupLabel) · 3. WithSeparator · 4. WithCheckboxItem (3 boolean toggles) · 5. WithRadioGroup (Light/Dark/System) · 6. WithIcons · 7. WithKeyboardShortcuts (label + kbd hint) · 8. NestedSubmenu (1 level deep) · 9. WithArrow · 10. DisabledItem · 11. PlacementSide · 12. RTL.

### ContextMenu (6)
13. Basic right-click area · 14. WithCheckboxItem · 15. NestedSubmenu · 16. WithDisabledItem · 17. CustomAnchor (right-click anywhere on a card) · 18. RTL.

## Aria-wiring (5 new)

1. Menu Trigger click → popup open + first Item focused.
2. Menu ArrowDown navigates items + ArrowUp loops.
3. CheckboxItem click → aria-checked flips + onCheckedChange fires.
4. RadioGroup ArrowDown moves selection between RadioItems.
5. ContextMenu right-click on Trigger area → popup open at pointer coords.

## Verification gates

A11y 215+ stories (197 + 18). Aria-wiring 75 PASS + 2 SKIP + 0 FAIL. 18 PNGs.

## Contingencies (decide inline)

- **Backdrop default**: SKIP (popover-feel). Opt-in subpart available.
- **Submenu placement**: side="right" by default; flips to left under viewport-edge.
- **CheckboxItem visual**: reuse Checkbox styling (accent fill when checked) so it reads consistent.
- **RadioItem visual**: reuse Radio dot pattern.
- **Forced-colors mirror**: every state selector at equal specificity. Slice 5/6/7 lesson.
- **Hit-target ≥ 1.75rem on items**: same floor as Select items.
- **LinkItem**: renders `<a>` natively; supports `href` + `target`. asChild via Slot for custom link components.

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave2-slice11`. Leave uncommitted.

## Report

Files changed; per-component API; token purity; tsc+build status; a11y count; aria-wiring; contingencies fired; 18 PNG paths; one taste note; "I did NOT commit, push, or merge."
