# Slice 11 visual polish — Menu + ContextMenu

**Worktree** `.worktrees/ui-design`. Slice 11 already merged at HEAD `84e8013e+`.

Codex visual review: 1🔴 + 3🟡 + 1🟢 = 5 items. Transcript at `/tmp/.../bsssk6u14.output:3931-4035`.

## Hard constraints

Standard. DO NOT commit; orchestrator will commit after verification.

## Fix list

### 🔴 (1)

**1. Plain Menu.Item children render into wrong grid lane — multi-word labels stack vertically.**
`Menu.tsx:268`, `Menu.css:166`.

`.zs-menu-item` grid is `indicator | text`, but plain `MenuItem` passes raw children — they land in the fixed indicator track. Fix structurally: wrap plain Item + LinkItem children in `.zs-menu-item__indicator` (empty) + `.zs-menu-item__text`. Plus CSS:

```css
.zs-menu-item {
  grid-template-columns: var(--zs-space-4) minmax(0, 1fr) auto;
  column-gap: var(--zs-space-2);
}
.zs-menu-item__text {
  min-inline-size: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
```

Apply to MenuItem, MenuLinkItem, MenuCheckboxItem, MenuRadioItem, MenuSubmenuTrigger so all share the 3-column lane.

### 🟡 (3)

**2. Keyboard shortcuts need a real trailing lane.**
`Menu.css:166`, `Menu.stories.tsx:305`.

The 3rd grid column (auto) added in fix 1 enables a `.zs-menu-item__shortcut` slot. Wire it as a documented prop or composable subpart. Add CSS:

```css
.zs-menu-item__shortcut {
  justify-self: end;
  margin-inline-start: var(--zs-space-6);
  font-family: var(--zs-font-mono);
  font-size: var(--zs-text-caption-1-size);
  line-height: var(--zs-text-caption-1-line);
  color: var(--zs-label-tertiary);
  white-space: nowrap;
}
```

Update WithKeyboardShortcuts story to use the new slot.

**3. Group labels too loud + ignore reserved indicator gutter.**
`Menu.css:304`.

```css
.zs-menu-group-label {
  padding-block: var(--zs-space-half) 0;
  padding-inline-start: calc(var(--zs-space-2) + var(--zs-space-4) + var(--zs-space-2));
  padding-inline-end: var(--zs-space-2);
  font-size: var(--zs-text-caption-2-size);
  line-height: var(--zs-text-caption-2-line);
  font-weight: var(--zs-text-caption-2-weight);
  color: var(--zs-label-tertiary);
}
```

If `--zs-text-caption-2-*` doesn't exist, fall back to caption-1 + smaller weight.

**4. Checked CheckboxItem too soft — should match Checkbox accent fill.**
`Menu.css:236`.

```css
.zs-menu-checkbox-item[data-checked] .zs-menu-item__indicator {
  background-color: var(--zs-accent);
  color: var(--zs-accent-ink);
  border-radius: var(--zs-radius-1);
  box-shadow: inset 0 0 0 var(--zs-selection-hairline) var(--zs-accent);
}
```

### 🟢 (1)

**5. Arrow under-defined.**
`Menu.css:146`.

```css
.zs-menu-arrow {
  color: var(--zs-surface);
  filter:
    drop-shadow(0 0 0 var(--zs-separator))
    drop-shadow(0 var(--zs-space-half) var(--zs-space-2) color-mix(in oklch, var(--zs-label) 14%, transparent));
}
```

## Files to modify

- `sdks/ui/src/components/Menu/Menu.tsx` — item 1 (wrap children in __indicator + __text); item 2 (expose shortcut slot prop or subpart).
- `sdks/ui/src/components/Menu/Menu.css` — items 1, 2, 3, 4, 5.
- `sdks/ui/src/stories/Menu.stories.tsx` — items 2 (rewire WithKeyboardShortcuts story).

## Verification

Builds + a11y + aria-wiring unchanged. Re-capture 12 Menu + 6 ContextMenu PNGs. Confirm plain items single-line; shortcuts right-aligned trailing column; group labels quieter and aligned to text lane; checked CheckboxItem reads as accent-fill chip.

## Report

End with files changed; per-item confirmation; build status; 18 PNG paths; "I did NOT commit, push, or merge."
