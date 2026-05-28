# Slice 10 review fixes — Popover + Tooltip

**Worktree** `.worktrees/wave1-slice10` on branch `wave1-slice10`.

Codex 1🔴+4🟡+2🟢 + Claude clean. The 🔴 (aria-wiring syntax) was fixed inline. Remaining: 4🟡 + 2🟢.

## Hard constraints

Standard. DO NOT commit; orchestrator merges.

## Fix list

### 🟡 (4)

**1. Tooltip Root delay default conflicts with Provider.**
`Tooltip.tsx:125, 172`.

Root defaults `delay=600`, Trigger forwards it. This makes `<Tooltip.Provider delay={...}>` ineffective because Base UI only uses Provider delay when Trigger delay is `undefined`. Fix: only pass Root delay when explicitly provided; let Base UI handle provider/default delay.

```tsx
// Before
const { delay = 600, ... } = rest;
// After
const { delay, ... } = rest;
// Pass delay only if explicitly set:
{delay !== undefined && <BaseTooltip.Trigger delay={delay} />}
```

**2. Arrow doesn't rotate per `data-side`.**
`Popover.css:152`, `Tooltip.css:96`, `Popover.tsx:269`, `Tooltip.tsx:280`.

Glyphs are fixed downward triangles; Base UI 1.5 positions arrows + emits `data-side` but doesn't rotate. Fix: add side-specific CSS rotations:
```css
.zs-popover-arrow[data-side="top"] svg { transform: rotate(0deg); }
.zs-popover-arrow[data-side="right"] svg { transform: rotate(90deg); }
.zs-popover-arrow[data-side="bottom"] svg { transform: rotate(180deg); }
.zs-popover-arrow[data-side="left"] svg { transform: rotate(270deg); }
```
Apply to both Popover and Tooltip Arrow.

**3. Tooltip Popup `id` override breaks owned `aria-describedby`.**
`Tooltip.tsx:225, 165-168`.

Trigger references `rootCtx.popupId`. A caller-supplied `<Tooltip.Popup id="custom-id">` leaves the trigger pointing at the missing rootCtx id. Fix: Root should own the shared id; OR Popup `id` override updates the context. Decision (autonomous): disallow Popup id override (omit `id` from `PopupProps`) — the shared id is internal contract.

**4. Forced-colors blocks don't mirror `data-starting-style` / `data-ending-style` / `data-side` states.**
`Popover.css:44`, `Tooltip.css:66`.

The state selectors set transitions/opacity/transform — not paint. So technically forced-colors doesn't NEED to mirror them (no token colors involved). Decision (autonomous): document this in CSS comment instead of mirroring — the state selectors set only motion/opacity which `prefers-reduced-motion: reduce` already handles. NOT a true fix; narrow the invariant.

### 🟢 (2)

**5. Popover modal default should be explicitly pinned.**
`Popover.tsx:87`.

Base UI 1.5 defaults `modal=false` but we should own the contract. Fix:
```tsx
const { modal = false, ...rest } = props;
return <BasePopover.Root modal={modal} {...rest} />;
```

**6. Tooltip `className` is documented as forwarded to Popup but Root sends it to BaseTooltip.Root which renders no DOM.**
`Tooltip.tsx:77, 133`.

Fix: carry className through context to Popup wrapper. OR remove the prop from the Root API and document that consumers should pass className on `<Tooltip.Popup>`. Decision (autonomous): remove from Root API; consumers use Popup's className directly.

## Files to modify

- `sdks/ui/src/components/Tooltip/Tooltip.tsx` — items 1, 3, 6.
- `sdks/ui/src/components/Tooltip/Tooltip.css` — item 2.
- `sdks/ui/src/components/Popover/Popover.tsx` — item 5.
- `sdks/ui/src/components/Popover/Popover.css` — items 2, 4 (comment-only).

## Verification gates

Standard. Token purity x5. Aria-wiring all PASS.

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice10`. Leave uncommitted.
