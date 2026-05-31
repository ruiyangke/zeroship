# Slice brief: PricingTable section

Second piece of the `sections/` layer (after Hero). A monetization band: a
responsive row of pricing **tiers**, each a card with a name, a price (amount +
period), a short description, a feature list (included / excluded conveyed by an
Icon + a visually-hidden word — never color alone), and a CTA. One tier may be
**featured** (highlighted + an optional "Most popular" badge). Composes the real
Card / Button / Icon (Lucide Check & Minus) / Badge / Stack / Cluster / Grid /
Separator. Lives in `sdks/ui/src/sections/PricingTable/`. High-value +
monetization-relevant → will get full dual review + visual review.

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
Run ALL commands with cwd INSIDE this worktree (it is nested under the main repo;
`pnpm --filter` from the wrong cwd builds the WRONG checkout — always `cd` in
first).
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference / compose (READ these first — mirror their APIs exactly)
- `sections/Hero/Hero.tsx` — the sections/-layer template: dual surface (props +
  compound parts), the recursive `flattenChildren` Fragment-descending walk, root
  data-slot override, the JSDoc density. **Mirror its shape.** In particular, the
  Hero fix established: descend Fragments in any child walk; never silently drop a
  compound child; keep comments honest about implemented behavior.
- `components/Card/Card.tsx` — each tier is a Card (use `variant`/elevation as it
  exposes); Card honors a consumer `data-slot` (default "card") — relabel tiers to
  the section's vocabulary. Heading-relevel pattern.
- `components/Icon/Icon.tsx` — `<Icon as={Check} size label?/>`; feature bullets
  use Lucide `Check` (included) and `Minus` (excluded), decorative (no label) since
  the visually-hidden word carries the meaning. Import from `lucide-react`.
- `components/Button/Button.tsx` — the CTA (consumer-supplied node, or ctaLabel).
- `components/Badge` — the "Most popular" featured badge (consumer passes the node
  or use the `badge` field).
- `layouts/{Grid,Stack,Cluster,Container}` — Grid for the tier row (responsive,
  collapses below a breakpoint); Stack for the in-card vertical rhythm.
- `_classnames`, `_slot`. House: forwardRef, data-slot, per-prop JSDoc.

## API (dual surface — props is the ergonomic 80%, compound for full control)
```ts
export interface PricingFeature {
  /** The feature text. */
  label: ReactNode;
  /** Included in this tier? Default true. false → Minus icon + "Not included". */
  included?: boolean;
  /** Optional trailing note (e.g. "up to 10k rows"). */
  note?: ReactNode;
}
export interface PricingTier {
  /** Stable id (React key + tier identity). */
  id: string;
  /** Tier name — renders as a heading (h3 by default, relevelable). */
  name: ReactNode;
  /** Price amount, already formatted (e.g. "$29", "Free", "Custom"). */
  price: ReactNode;
  /** Period suffix beside the price (e.g. "/mo"). Optional. */
  period?: ReactNode;
  /** One-line positioning under the price. */
  description?: ReactNode;
  /** Feature list. */
  features?: PricingFeature[];
  /** CTA — a consumer Button node. If omitted, falls back to ctaLabel. */
  cta?: ReactNode;
  /** CTA label when `cta` not supplied → renders a default Button. */
  ctaLabel?: ReactNode;
  /** Click handler for the default (ctaLabel) Button. */
  onCtaClick?: () => void;
  /** Highlight this tier (elevated/ringed) + reorder emphasis. */
  featured?: boolean;
  /** Small badge on a featured tier (e.g. <Badge>Most popular</Badge> or text). */
  badge?: ReactNode;
}
export interface PricingTableProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /** Ergonomic mode: the tiers, left→right. */
  tiers?: PricingTier[];
  /** Section heading id target / column count override is derived from tiers. */
  /** Container width. Default "lg". */
  size?: ContainerSize;
  /** Heading-level treatment for tier names. Default "h3". (Document; keep simple.) */
  children?: ReactNode;     // compound PricingTable.Tier / .Feature parts
  "data-slot"?: string;
}
// Compound: PricingTable.Tier (props: name/price/period/description/featured/badge/
//   cta…) + PricingTable.Feature (label/included/note) for full composition.
```
**Dual surface is ADDITIVE** (Hero rule): `tiers` prop renders first, then any
compound `<PricingTable.Tier>` children fall through after — no suppression. Use a
recursive `flattenChildren` (Fragment-descending) walk if you need to detect/lift
compound parts. If you don't need to lift anything (tiers just render in order),
still descend Fragments for the compound-tier path.

## Layout / structure
- Root `<section data-slot="pricing-table">` → `Container` → a `Grid` of tier
  cards. Grid is responsive: N columns at wide widths (N = tier count, capped
  sensibly), collapsing to a single stacked column below `--zs-bp-md` (mirror the
  Hero/Split collapse — use `47.999rem` to match the package). Equal-height cards.
- Each tier = a `Card` (`data-slot="pricing-tier"`): header (badge if featured →
  name heading → price line `price` + muted `period` → description), a `Separator`,
  the feature `<ul>`, then the CTA pinned to the card bottom (so CTAs align across
  tiers of differing feature counts — use a Stack that pushes the CTA down, e.g.
  the feature list grows and the CTA sits in a bottom row).
- **Featured tier**: visually elevated/ringed (accent border or raised elevation
  via Card variant + a token accent ring), carries the `badge`. Keep it
  token-pure; no raw color.
- Feature row: `Icon` (Check included / Minus excluded) + the label + optional
  muted note. Excluded features read muted. **Inclusion is conveyed by the icon +
  a `.zs-visually-hidden` word ("Included" / "Not included"), NOT color/icon
  alone** (color-blind + SR users).

## a11y
- Each tier name is a heading (`<h3>` default; document a relevel path if trivial,
  else keep h3 — the section is a band under a page `<h2>`/`<h1>`). The section is
  a labelled region only if a section-level heading exists — **this section has no
  single headline** (it's a row of tiers), so do NOT emit a dangling
  `aria-labelledby`. If you add an optional section `title`/`description` lead-in,
  THEN label the region by it (gate the attr on the heading actually rendering —
  the Hero R6 lesson). Otherwise leave the `<section>` unlabelled (a consumer wraps
  it under their page heading).
- Feature list is a real `<ul>`/`<li>`. Inclusion conveyed beyond color (icon +
  visually-hidden word). CTAs are real `<Button>`s with discernible text.
- Featured tier: the badge text ("Most popular") is real visible text — sufficient;
  do not rely on visual treatment alone to convey "recommended".
- forced-colors: tier borders / featured ring / icons stay distinguishable
  (CanvasText / appropriate system colors); reduced-motion block present (no-op
  parity if no motion, per AuthForm/Hero convention).

## Constraints
`--zs-*` only; no raw hex/px (rem/%/dvh/oklch ok); logical properties; forced-colors
+ prefers-reduced-motion blocks (both required); no "HIG"/"Apple"; pre-launch
no-back-compat (rename/break freely, no shims/aliases). forwardRef + data-slot
vocab (pricing-table / pricing-tier / pricing-tier-header / pricing-price /
pricing-features / pricing-feature / pricing-cta — pick a clean set, document it).
Export from `sections/index.ts`; `@import` CSS into styles.css under the existing
"Page sections" group; story `title: "Sections/PricingTable"`, `layout:
"fullscreen"`.

## Stories (`src/stories/PricingTable.stories.tsx`)
- **ThreeTiers** (Free / Pro[featured, "Most popular" badge] / Enterprise — each
  with price+period, description, 4-6 features incl. some excluded, a CTA Button).
- **Compound** (the same expressed via `<PricingTable.Tier>` + `<PricingTable.Feature>`
  parts — proves the compound surface + additive fall-through).
- **TwoTiers** (Free / Pro — proves the responsive column count adapts).
- **FeaturedHighlight** (showcase the featured treatment + badge).
- **play()**: assert tier names are headings; the feature list is a `<ul>` and an
  excluded feature exposes the "Not included" visually-hidden word (query it);
  a CTA Button is present + clickable (spy on onCtaClick for the ctaLabel path);
  the featured tier renders its badge text. axe clean on all non-degenerate stories.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
node -e "const x=require('./sdks/ui/dist/index.js'); console.log('PricingTable export:', !!x.PricingTable)"
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' sections/PricingTable|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' sections/PricingTable|wc -l)"
grep -c 'prefers-reduced-motion' sections/PricingTable/PricingTable.css   # >=1
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6289 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6289 --maxWorkers=1 PricingTable.stories 2>&1 | grep -E 'Tests:|✕'
```
Report: files, the dual-surface + additive fall-through (Fragment-descending walk),
the equal-height/CTA-pinned layout, the color-not-alone inclusion conveyance, the
no-dangling-label decision, featured-tier treatment, build/grep/suite counts,
decisions. This is a flagship visual section — make it genuinely polished
(orchestrator will screenshot-review + dual-review).
