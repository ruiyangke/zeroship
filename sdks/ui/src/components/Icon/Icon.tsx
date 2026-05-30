/*
 * Icon — a governed wrapper around a single SVG icon component.
 *
 * The intended source is lucide-react: consumers `import { Search }
 * from "lucide-react"` and pass the component via `as`:
 *
 *   <Icon as={Search} label="Search" />
 *
 * Why `as={Component}` and not `name="search"`:
 *
 *   - Tree-shakeable. Only the icons a bundle actually references ship.
 *     A `name`-based string map would pull the entire icon set into
 *     every build. The Builder agent emits real imports, so it follows
 *     the same tree-shaking path consumers get.
 *   - Type-safe. The compiler verifies `as` is a real SVG component
 *     instead of trusting a free-form string against a runtime map.
 *
 * Design guarantees encoded here (in source so they travel with the
 * code, not a sibling doc):
 *
 *   1. Token-pure sizing. The size is governed by CSS — the
 *      `zs-icon--{size}` class sets `inline-size`/`block-size` to a
 *      `--zs-icon-*` token. We deliberately do NOT forward a numeric
 *      `size` prop to the Lucide component (Lucide would stamp a raw
 *      `width`/`height="24"` px attribute on the svg). Letting CSS own
 *      the dimension keeps the surface token-pure and lets a consumer
 *      override with any CSS length via `style` / a className.
 *
 *   2. currentColor. Lucide draws with `stroke="currentColor"` by
 *      default, so the icon inherits the surrounding text color with no
 *      color prop. Consumers tint by setting `color` on the parent
 *      (e.g. a Button, a status row). One fewer knob, one fewer way to
 *      drift off-palette.
 *
 *   3. a11y is a single decision driven by `label`:
 *        - `label` set    → meaningful icon: `role="img"` +
 *          `aria-label={label}`. AT announces the name.
 *        - `label` omitted → decorative icon: `aria-hidden="true"`.
 *          AT skips it. This is the 80% case — an icon sitting beside
 *          its own text label (in a Button, a menu row, a banner).
 *      `focusable="false"` is always set so IE/legacy-Edge don't put
 *      the svg in the tab order; modern engines ignore it harmlessly.
 *
 *   4. forced-colors needs no special handling — because the icon paints
 *      with `currentColor`, the system high-contrast palette maps it to
 *      the surrounding text color automatically.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentType,
  type Ref,
  type SVGProps,
} from "react";
import { classnames } from "../_classnames";

export type IconSize = "sm" | "md" | "lg";

export interface IconProps
  extends Omit<ComponentPropsWithoutRef<"svg">, "ref"> {
  /**
   * The icon component to render — a lucide-react icon (e.g. `Search`)
   * or any component that takes `SVGProps<SVGSVGElement>` and renders an
   * `<svg>`. Passed as the component itself, not a name string, so only
   * the icons a bundle references ship (tree-shakeable + type-safe).
   */
  as: ComponentType<SVGProps<SVGSVGElement>>;
  /**
   * Token size — `sm` (1rem), `md` (1.25rem, default), `lg` (1.5rem).
   * Sizing is applied via CSS class, not a raw px prop, so it stays
   * token-pure. To use an arbitrary length, pass a `style` override
   * (e.g. `style={{ inlineSize: "2rem", blockSize: "2rem" }}`).
   */
  size?: IconSize;
  /**
   * Accessible name. When provided, the icon is treated as meaningful:
   * it gets `role="img"` + `aria-label={label}` so assistive tech
   * announces it. When omitted (the default), the icon is decorative
   * and gets `aria-hidden="true"` — AT skips it. Most catalog icons sit
   * beside their own visible text and should stay decorative; set
   * `label` only when the icon is the sole carrier of meaning (e.g. an
   * icon-only button).
   */
  label?: string;
}

/**
 * The rendered `<svg>`. `forwardRef` targets the svg element so callers
 * can measure / animate it. The a11y attributes are reasserted AFTER
 * the `...rest` spread so a caller can't accidentally clobber the
 * `label`-driven semantics (e.g. spreading an `aria-hidden` over a
 * labelled, meaningful icon).
 */
export const Icon = forwardRef<SVGSVGElement, IconProps>(function Icon(
  { as: Component, size = "md", label, className, ...rest },
  ref,
) {
  const composedClassName = classnames(
    "zs-icon",
    `zs-icon--${size}`,
    className,
  );

  const meaningful = label != null && label !== "";

  return (
    <Component
      {...rest}
      ref={ref as Ref<SVGSVGElement>}
      className={composedClassName}
      data-slot="icon"
      focusable="false"
      // a11y decision, reasserted after the spread so caller props can't
      // contradict it: a name means role="img"; no name means hidden.
      role={meaningful ? "img" : undefined}
      aria-label={meaningful ? label : undefined}
      aria-hidden={meaningful ? undefined : true}
    />
  );
});
Icon.displayName = "Icon";
