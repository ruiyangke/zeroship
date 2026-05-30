# Visual-polish brief: AppShell FullShell (codex visual review)

From the codex visual review of the redesigned AppShell (APPROVE-WITH-NITS). All
🟡/🟢 calibration in the SHOWCASE demo (`stories/AppShell.stories.tsx`,
`FullShell` + the nav-item recipe `<style>`). Component API/structure unchanged.
`--zs-*` tokens only, no raw hex/px. All 4 AppShell `play()`s must still pass.

> **DO NOT commit, push, or merge.** Apply + self-verify + report.

1. **🟡 Dedupe the "New project" CTA / one action zone.** It currently appears
   in BOTH the global header and the PageHeader. Keep the **primary blue
   "New project" in the PageHeader** (page-scoped). In the **header**, replace it
   with a *global* control: an icon-only ghost/plain `Button` (e.g. a search or
   notifications glyph — inline `aria-hidden` SVG) WITH an `aria-label`, placed
   before the `Avatar`. So: header = workspace/account zone (brand · [icon
   action] · avatar); page header = page actions. Don't leave the header end
   empty/lonely.
2. **🟡 Soften the active nav item.** Today it stacks four signals (blue text +
   blue icon + filled tint + left rail) — too loud for crystal. Keep ONE
   dominant signal: keep the **leading accent rail** + a **lighter fill**
   (reduce the `color-mix` accent share, e.g. ~14%→~8%), and soften the ink to a
   slightly calmer accent while staying AA (it must remain legible on the
   lighter fill). One clearly-active item; not shouty.
3. **🟡 Make the StatCards match the activity card's weight.** The stat cards
   read more elevated/"cardy" than the flatter Recent-activity card. Bring them
   into one system: give the StatCards a flatter Card surface (e.g. the
   `surface`/`outline` Card variant rather than elevated) — or match the
   activity card to them — so the stat row doesn't out-prominence the main
   content. Pick the calmer direction (flatter stats).
4. **🟢 A little more vertical air** between the stat row and the activity card
   (bump the Main content `Stack` gap a step), and/or slightly reduce stat-card
   height so the row reads less blocky.
5. **🟢 Breadcrumb** stays understated — ensure it's clearly lighter/smaller
   than the page title (it should already be footnote; just confirm the title is
   the anchor). No change if already so.

a11y: the icon-only header button needs an `aria-label`; active nav item keeps
`aria-current="page"`; icons `aria-hidden`. Keep the two disabled landmark axe
rules + their comment; introduce no new violations.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.tsx' --include='*.css' layouts/AppShell stories/AppShell.stories.tsx|wc -l)/$(grep -rEn '[0-9]+px' --include='*.tsx' --include='*.css' layouts/AppShell stories/AppShell.stories.tsx|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6229 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6229 --maxWorkers=1 AppShell.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server is on :6006 — use 6229.) Report each change (old→new), build, grep
counts (0/0), suite pass count.
