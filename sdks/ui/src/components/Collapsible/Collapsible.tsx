/*
 * Collapsible — Root + Trigger + Panel.
 *
 * Wraps Base UI's `Collapsible` primitive. Where Accordion stacks
 * disclosure rows under a shared root, Collapsible is the single-row
 * version — one Trigger toggles one Panel. Reads as a "Show more / hide"
 * affordance on a row, a "Show advanced" toggle on a form, or a
 * lightweight inline disclosure inside a Card.
 *
 *   - Collapsible.Root: owns controlled / uncontrolled `open`, disabled
 *     state. The Root is a `<div>` so it can be styled as a card or
 *     wrap inline content.
 *
 *   - Collapsible.Trigger: a real `<button type="button">` with
 *     `aria-expanded` and `aria-controls` wired to its Panel. The
 *     `type="button"` stamp is unconditional and not overridable
 *     from the public prop surface, so a Collapsible inside a Form
 *     never submits it. Disabled triggers don't respond to clicks
 *     and read inactive.
 *
 *   - Collapsible.Panel: a `<div>` carrying the matching `id`. Base
 *     UI tags it with `data-open` / `data-closed` plus the transient
 *     `data-starting-style` and `data-ending-style` attributes while
 *     the panel animates in / out. Our CSS keys block-size off those
 *     attributes — `0` when closed (and during the starting / ending
 *     transition frames), `var(--collapsible-panel-height)` when open.
 *     `prefers-reduced-motion: reduce` snaps without a transition. The
 *     Panel is `[hidden]` when fully closed so its content stays out
 *     of the tab order.
 *
 * Design guarantees encoded in source:
 *
 *   1. Trigger is ALWAYS a `<button type="button">`. We stamp `type`
 *      unconditionally on the JSX and omit it from the public props so
 *      a consumer cannot spread `type="submit"`. Same defense Accordion
 *      / Tabs / Toggle apply — a Collapsible inside a Form must NOT
 *      submit.
 *
 *   2. `disabled` cascades to the Trigger so the affordance reads
 *      inactive without the consumer threading the prop through twice.
 *
 *   3. Panel animates via Base UI's `--collapsible-panel-height` CSS
 *      var, paired with `prefers-reduced-motion` to flatten the
 *      animation. The chevron rotation on the Trigger also flattens in
 *      reduced motion.
 *
 *   4. Forced colors: the Trigger ink resolves to `CanvasText` /
 *      `HighlightText` (open state), focus to `Highlight`. The Panel
 *      content stays on `CanvasText` for legibility.
 *
 *   5. RTL via logical properties. The chevron lives at `inset-inline-
 *      end` so RTL swaps it automatically.
 *
 *   6. Compound namespace `Collapsible.{Root,Trigger,Panel}` — calling
 *      `<Collapsible>` directly mounts Root.
 *
 *   7. No `asChild` surface. Base UI's `render` prop is omitted from
 *      every part — Collapsible's Trigger is a `<button>` with the
 *      platform-managed `aria-controls`, so there is no customization
 *      path the design system supports via a Slot. Composition is via
 *      `children`. If a future slice needs `asChild` here, it must add
 *      it explicitly through `_slot.ts` so the slot ref / event
 *      composition stays consistent across the design system.
 */
import {
  forwardRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Collapsible as BaseCollapsible } from "@base-ui/react/collapsible";
import { classnames } from "../_classnames";

/* ─── Collapsible root ──────────────────────────────────────────────── */

type BaseCollapsibleRootProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Root
>;

export interface CollapsibleRootProps
  extends Omit<
    BaseCollapsibleRootProps,
    | "render"
    | "className"
    | "open"
    | "defaultOpen"
    | "onOpenChange"
    | "disabled"
    | "children"
  > {
  /**
   * Controlled open state. When provided, `onOpenChange` MUST also be
   * provided to react to user-driven toggles. Omit both to use
   * `defaultOpen` for uncontrolled.
   */
  open?: boolean;
  /**
   * Uncontrolled initial open state. Pair with `onOpenChange` to
   * observe changes without taking control.
   *
   * @default false
   */
  defaultOpen?: boolean;
  /** Called when the open state changes. */
  onOpenChange?: (open: boolean) => void;
  /**
   * Whether the component should ignore user interaction. Cascades to
   * the Trigger so the affordance reads inactive.
   *
   * @default false
   */
  disabled?: boolean;
  /** Optional class hook on the root container. */
  className?: string;
  /** Collapsible children — `<Collapsible.Trigger>` + `<Collapsible.Panel>`. */
  children: ReactNode;
}

const CollapsibleRoot = forwardRef<HTMLDivElement, CollapsibleRootProps>(
  function CollapsibleRoot(
    {
      open,
      defaultOpen = false,
      onOpenChange,
      disabled = false,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    return (
      <BaseCollapsible.Root
        {...rest}
        ref={ref}
        open={open}
        defaultOpen={defaultOpen}
        onOpenChange={
          onOpenChange
            ? (next) => onOpenChange(next)
            : undefined
        }
        disabled={disabled}
        className={classnames("zs-collapsible", className)}
      >
        {children}
      </BaseCollapsible.Root>
    );
  },
);
(CollapsibleRoot as { displayName?: string }).displayName =
  "Collapsible.Root";

/* ─── Collapsible.Trigger ───────────────────────────────────────────── */

type BaseCollapsibleTriggerProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Trigger
>;

export interface CollapsibleTriggerProps
  extends Omit<
    BaseCollapsibleTriggerProps,
    "render" | "className" | "type"
  > {
  /** Optional class hook on the trigger button. */
  className?: string;
}

const CollapsibleTrigger = forwardRef<
  HTMLButtonElement,
  CollapsibleTriggerProps
>(function CollapsibleTrigger({ className, children, ...rest }, ref) {
  return (
    <BaseCollapsible.Trigger
      {...rest}
      ref={ref}
      // Stamp type="button" UNCONDITIONALLY. `type` is omitted from
      // CollapsibleTriggerProps so a caller can't override the stamp
      // by spreading `type="submit"` through `...rest`. Mirrors the
      // Accordion / Tabs form-safety rule.
      type="button"
      className={classnames("zs-collapsible-trigger", className)}
    >
      <span className="zs-collapsible-trigger-label">{children}</span>
      <CollapsibleChevron />
    </BaseCollapsible.Trigger>
  );
});
(CollapsibleTrigger as { displayName?: string }).displayName =
  "Collapsible.Trigger";

function CollapsibleChevron() {
  return (
    <svg
      className="zs-collapsible-trigger-chevron"
      viewBox="0 0 12 12"
      aria-hidden="true"
      focusable="false"
    >
      <path
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        d="M3 4.5l3 3 3-3"
      />
    </svg>
  );
}

/* ─── Collapsible.Panel ─────────────────────────────────────────────── */

type BaseCollapsiblePanelProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Panel
>;

export interface CollapsiblePanelProps
  extends Omit<BaseCollapsiblePanelProps, "render" | "className"> {
  /** Optional class hook on the panel wrapper. */
  className?: string;
}

const CollapsiblePanel = forwardRef<HTMLDivElement, CollapsiblePanelProps>(
  function CollapsiblePanel({ className, children, ...rest }, ref) {
    return (
      <BaseCollapsible.Panel
        {...rest}
        ref={ref}
        className={classnames("zs-collapsible-panel", className)}
      >
        {/* Inner wrapper carries the padding so the outer block-size
            animation cleanly drives `block-size: 0`. */}
        <div className="zs-collapsible-panel-inner">{children}</div>
      </BaseCollapsible.Panel>
    );
  },
);
(CollapsiblePanel as { displayName?: string }).displayName =
  "Collapsible.Panel";

/* ─── Compose the public namespace ──────────────────────────────────── */

const ForwardedCollapsible = CollapsibleRoot as typeof CollapsibleRoot & {
  Root: typeof CollapsibleRoot;
  Trigger: typeof CollapsibleTrigger;
  Panel: typeof CollapsiblePanel;
};
ForwardedCollapsible.Root = CollapsibleRoot;
ForwardedCollapsible.Trigger = CollapsibleTrigger;
ForwardedCollapsible.Panel = CollapsiblePanel;
(ForwardedCollapsible as { displayName?: string }).displayName = "Collapsible";

export const Collapsible = ForwardedCollapsible;
export { CollapsibleRoot, CollapsibleTrigger, CollapsiblePanel };
