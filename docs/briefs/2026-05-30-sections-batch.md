# Slice brief: CTA + StatsBand + FAQ + Footer (final sections/ batch)

The last four `sections/`-layer bands, built in one slice (all simple/compositional,
mirroring the established Hero/PricingTable/FeatureGrid patterns). Each lives in its
own `sdks/ui/src/sections/<Name>/` dir with `{<Name>.tsx, <Name>.css, index.ts}`,
is exported from `sections/index.ts`, `@import`s its CSS into styles.css under the
"Page sections" group, and gets a `stories/<Name>.stories.tsx` (title
`Sections/<Name>`, `layout: "fullscreen"`).

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Run ALL commands with cwd INSIDE it (nested under main repo; wrong cwd builds the
WRONG checkout — `cd` in first).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Shared conventions (ALL four — read Hero/PricingTable/FeatureGrid first, mirror exactly)
- `--zs-*` tokens only; NO raw hex/px (rem/%/dvh/oklch); logical properties;
  forced-colors + prefers-reduced-motion blocks (BOTH required, parity no-op OK);
  no "HIG"/"Apple"; pre-launch no-back-compat. forwardRef + data-slot vocab +
  per-prop JSDoc. Compose Container/Stack/Cluster/Grid + existing components.
- Where a band has a header/title, the `<section>` is `aria-labelledby` it ONLY
  when the title renders (the `titleRenders = title != null && title !== false &&
  title !== ""` guard) — NO dangling label when headerless. Heading is `<h2>`.
- Dual surface (props + compound parts, additive via the recursive
  Fragment-descending `flattenChildren` walk) ONLY where a band has a repeating
  list (StatsBand items, FAQ items, Footer columns). CTA has no list → props only.
- Root `data-slot` overridable (default the band name). Each band paints no heavy
  background by default (composable) unless noted.

## 1. CTA — `sections/Cta/` (data-slot "cta")
A focused call-to-action band: optional eyebrow + `<h2>` title + description +
an actions `Cluster` (consumer Buttons). Centered by default; `align="start"`.
```ts
interface CtaProps extends Omit<ComponentPropsWithoutRef<"section">,"title"> {
  eyebrow?: ReactNode; title?: ReactNode; description?: ReactNode;
  actions?: ReactNode; align?: "center"|"start"; size?: ContainerSize;
  /** Optional surface treatment: "plain" (default, transparent) | "tinted"
   *  (a subtle accent-tint panel with radius + padding — a contained CTA card). */
  variant?: "plain"|"tinted";
  children?: ReactNode; "data-slot"?: string;
}
```
Stack: eyebrow → title → description → actions Cluster (aligned per `align`).
`tinted` wraps the body in a rounded accent-tint panel (token color-mix idiom).
data-slot: cta / cta-title / cta-description / cta-actions.
Stories: Default (eyebrow+title+desc+2 Buttons, centered), Tinted, StartAligned,
Minimal (title + 1 Button). play: title is <h2> + section labelled by it; a CTA
Button present/clickable; headerless-not-applicable (CTA always has a title — but
still guard). axe clean.

## 2. StatsBand — `sections/StatsBand/` (data-slot "stats-band")
A horizontal band of headline metrics (value + label, optional description).
Dual surface: `stats` prop + compound `StatsBand.Stat`. Composes Grid.
```ts
interface StatItem { id?: string; value: ReactNode; label: ReactNode; description?: ReactNode; }
interface StatsBandProps extends Omit<ComponentPropsWithoutRef<"section">,"title"> {
  eyebrow?: ReactNode; title?: ReactNode; description?: ReactNode;   // optional header
  stats?: StatItem[]; columns?: 2|3|4;       // default = stats.length capped 4
  align?: "center"|"start"; size?: ContainerSize; children?: ReactNode; "data-slot"?: string;
}
// Compound: StatsBand.Stat (value/label/description) — additive Fragment-walk.
```
Each stat: a large `value` (display/title-1 type scale, accent or label ink) over a
muted `label`, optional small description. Responsive Grid, collapses below
--zs-bp-md (Grid owns it). data-slot: stats-band / stats-band-header / -title /
-items / -stat / -stat-value / -stat-label / -stat-description.
a11y: values+labels are plain text (not headings — a stats row isn't a heading
outline); if a header title renders, section labelled by it (gated). Each stat is a
group; the value's unit/meaning is in the visible label (color-not-alone N/A here).
Stories: ThreeStats (header + 3), FourStats, Compound (prop + StatsBand.Stat
additive), Headerless (no aria-labelledby). play: stats render value+label; compound
additive order; headerless no label. axe clean.

## 3. Faq — `sections/Faq/` (data-slot "faq")
A frequently-asked-questions band: optional header + a disclosure list of Q/A.
COMPOSE THE EXISTING `Accordion` component (read components/Accordion) — FAQ is an
Accordion of question→answer; do NOT re-roll disclosure. Dual surface: `items` prop
+ compound `Faq.Item`.
```ts
interface FaqEntry { id?: string; question: ReactNode; answer: ReactNode; }
interface FaqProps extends Omit<ComponentPropsWithoutRef<"section">,"title"> {
  eyebrow?: ReactNode; title?: ReactNode; description?: ReactNode;
  items?: FaqEntry[];
  /** Allow multiple open at once (Accordion openMultiple). Default false. */
  multiple?: boolean;
  size?: ContainerSize; children?: ReactNode; "data-slot"?: string;
}
// Compound: Faq.Item (question/answer) — additive Fragment-walk.
```
Render an `Accordion` (mirror its API — openMultiple, item triggers = the question
as the trigger heading, panel = the answer). The question heading level inside the
Accordion item should sit under the section <h2> (so the Accordion triggers are
within an <h3>-level region if Accordion supports a headingLevel; else follow
Accordion's default — check). Constrain to a readable measure (max-inline-size).
data-slot: faq / faq-header / faq-title / faq-list / faq-item.
a11y: the Accordion already provides the disclosure a11y (button triggers,
aria-expanded, region). Section labelled by its <h2> when present (gated). The
answer content is a real region. Stories: Default (header + 4 Q/A, single-open),
Multiple (openMultiple), Compound (prop + Faq.Item additive), Headerless. play:
clicking a question expands its answer (assert via the real Accordion behavior —
aria-expanded + answer visible); compound additive; headerless no label. axe clean.

## 4. Footer — `sections/Footer/` (data-slot "footer", root renders <footer>)
A site footer: a brand/blurb block + columns of link groups + a bottom bar
(copyright + optional secondary links/social slot). Dual surface: `columns` prop +
compound `Footer.Column`. Root is a real `<footer>` landmark (contentinfo).
```ts
interface FooterLink { label: ReactNode; href: string; }
interface FooterColumn { id?: string; title: ReactNode; links: FooterLink[]; }
interface FooterProps extends ComponentPropsWithoutRef<"footer"> {
  brand?: ReactNode;          // logo/wordmark slot
  description?: ReactNode;    // short blurb under the brand
  columns?: FooterColumn[];   // link groups
  copyright?: ReactNode;      // bottom-bar left
  actions?: ReactNode;        // bottom-bar right (social icons / secondary links slot)
  size?: ContainerSize; children?: ReactNode; "data-slot"?: string;
}
// Compound: Footer.Column (title + links, or children) — additive Fragment-walk.
```
Layout: a top row = brand+description column beside the link-group columns (Grid or
flex, collapses below --zs-bp-md to stacked), a `Separator`, then a bottom bar
(copyright start, actions end; wraps on narrow). Each column: a `<h3>`/strong title
+ a `<ul>` of `<a>` links (real anchors). Links use the house link treatment.
data-slot: footer / footer-brand / footer-description / footer-columns / footer-column
/ footer-column-title / footer-links / footer-bottom / footer-copyright / footer-actions.
a11y: root `<footer>` (contentinfo landmark — exactly one per page; the band IS the
footer so that's correct). Column titles are headings; link lists are real <ul>/<li>
of <a href>. forced-colors: text/links/separator legible. Stories: Default
(brand+blurb + 3 link columns + copyright + a couple social links), Compound (prop +
Footer.Column additive), Minimal (just brand + copyright, no columns). play: footer
is a contentinfo landmark; links are real anchors with hrefs; column titles are
headings; compound additive. axe clean.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
node -e "const x=require('./sdks/ui/dist/index.js'); console.log('exports:', !!x.Cta, !!x.StatsBand, !!x.Faq, !!x.Footer)"
cd sdks/ui/src && for d in Cta StatsBand Faq Footer; do echo "$d hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' sections/$d|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' sections/$d|wc -l) rm:$(grep -c 'prefers-reduced-motion' sections/$d/$d.css)"; done
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
for p in 6364; do fuser -k $p/tcp 2>/dev/null; done; sleep 1
(npx http-server storybook-static -p 6364 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6364 --maxWorkers=2 "(Cta|StatsBand|Faq|Footer).stories" 2>&1 | grep -E 'Tests:|Test Suites:|✕'
fuser -k 6364/tcp 2>/dev/null
```
Report per section: files, dual-surface/additive (where applicable), the
title-gated/no-dangling-label decision, FAQ composing the real Accordion, Footer's
contentinfo landmark, build/grep/suite counts, decisions. All four are visible →
orchestrator will screenshot-review.
