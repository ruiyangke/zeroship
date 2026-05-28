/*
 * Accordion — Item + Header + Trigger + Panel anatomy.
 *
 * Wraps Base UI's `Accordion` primitive. Reads as a stack of disclosure
 * rows: each Item exposes a Trigger (a heading-button) and a Panel that
 * animates open/closed. The discriminated union on the root distinguishes
 * "one open at a time" (single) from "any number open" (multiple) — the
 * mode picks the value shape so the contract stays sharp.
 *
 *   - Accordion.Root: owns mode (`single` vs `multiple`), controlled /
 *     uncontrolled value, orientation, disabled cascade. Children declare
 *     their own anatomy via `<Accordion.Item>` blocks.
 *
 *   - Accordion.Item: groups a Header+Panel pair. Each Item has its own
 *     `value` key. When the root is `single` the open value is a single
 *     string; when `multiple` it's an array of strings.
 *
 *   - Accordion.Header: renders an `<h3>` by default — Base UI promotes
 *     each Trigger button to a heading so screen-reader landmark
 *     navigation lands on each row.
 *
 *   - Accordion.Trigger: a real `<button type="button">` with
 *     `aria-expanded` and `aria-controls` wired to its Panel. The
 *     `type="button"` stamp is unconditional and not overridable from
 *     the prop surface, so an accordion inside a Form never submits
 *     it. We never attach a raw `onClick` that fights Base UI's
 *     controlled state — mode + value flow through the root. Disabled
 *     triggers are skipped by roving (ArrowDown/ArrowUp focus
 *     traversal).
 *
 *   - Accordion.Panel: a `<div>` carrying the matching `id` plus
 *     `aria-labelledby` pointing at its Trigger. Base UI emits the
 *     panel's natural block-size on the `--accordion-panel-height` CSS
 *     custom property and tags the panel with `data-open`,
 *     `data-starting-style`, and `data-ending-style` while transitions
 *     run; our CSS keys block-size off those attributes. The closed
 *     panel resolves to `0`. `prefers-reduced-motion: reduce` snaps
 *     without a transition.
 *
 * Design guarantees encoded in source (so they travel with the code):
 *
 *   1. Discriminated union on `type`. There is no `expanded` boolean on
 *      a multiple-mode root — the mode literally picks the value shape.
 *      Single mode accepts `collapsible` (can the only-open item be
 *      closed? default false, mirroring radio-group semantics); multiple
 *      mode has no `collapsible` knob because "no items open" is always
 *      reachable.
 *
 *   2. Trigger is ALWAYS a `<button type="button">`. We stamp `type`
 *      unconditionally on the JSX and omit it from the public props so
 *      a consumer cannot spread `type="submit"` onto a disclosure
 *      trigger — an accordion inside a Form must never submit it.
 *      Header wraps the trigger in an `<h3>` so the heading-level
 *      navigation works. We do NOT nest a `<button>` inside the Trigger
 *      content (it's already a button) — that would create an invalid
 *      focusable.
 *
 *   3. Orientation. Vertical is the default (the conventional accordion
 *      stack); `horizontal` rotates the roving keys to Left/Right and
 *      our chevron rotation axis stays consistent.
 *
 *   4. Reduced motion. The panel transition is suppressed inside
 *      `prefers-reduced-motion: reduce`. The chevron rotation is also
 *      flattened (`transform: none`) so the open-state cue arrives
 *      instantly without animation.
 *
 *   5. Forced colors. The trigger ink + separator hairlines collapse to
 *      `CanvasText`; the focus ring resolves to `Highlight`. Open-state
 *      backgrounds map to `Highlight`/`HighlightText` so the
 *      currently-expanded row stays visible in high-contrast mode.
 *
 *   6. RTL via logical properties everywhere. No `left`/`right` —
 *      everything is `inset-inline-*` / `padding-inline-*`. The chevron
 *      lives at `inset-inline-end` so RTL automatically swaps it.
 *
 *   7. Compound namespace exported as `Accordion.{Root,Item,Header,
 *      Trigger,Panel}`. The root is also the default entry — calling
 *      `<Accordion>` directly mounts Root.
 *
 *   8. No `asChild` surface. Base UI's `render` prop is omitted from
 *      every part — Accordion's Trigger is invariably a heading-button
 *      and the Panel carries the platform-managed `aria-labelledby`,
 *      so there is no customization path the design system supports
 *      via a Slot. Composition is via `children`. If a future slice
 *      needs `asChild` here (e.g. wrapping the Trigger in a custom
 *      button), it must add it explicitly through `_slot.ts` so the
 *      slot ref / event composition stays consistent across the
 *      design system.
 */
import {
  createContext,
  forwardRef,
  useContext,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Accordion as BaseAccordion } from "@base-ui/react/accordion";
import { classnames } from "../_classnames";

export type AccordionOrientation = "vertical" | "horizontal";

/* ─── root-local context ────────────────────────────────────────────── *
 *
 * Carries the resolved orientation so child parts (Item, Trigger, Panel)
 * can scope to it via classNames without re-reading Base UI's state.
 * Vertical default mirrors Base UI's own default. */
interface AccordionContextValue {
  orientation: AccordionOrientation;
}

const AccordionContext = createContext<AccordionContextValue | null>(null);

function useAccordionContext(): AccordionContextValue {
  return useContext(AccordionContext) ?? { orientation: "vertical" };
}

/* ─── Accordion root — discriminated union ──────────────────────────── *
 *
 * The `type` prop picks the value shape:
 *
 *   single   → value is a single string (or undefined when closed).
 *              `collapsible: true` lets the open item be re-clicked to
 *              close itself; default `false` mirrors RadioGroup (one
 *              item is always selected once one is opened).
 *
 *   multiple → value is a string[] (empty when nothing open). No
 *              `collapsible` knob — "no items open" is always
 *              reachable by toggling.
 *
 * Both modes accept `defaultValue` (uncontrolled) and `onValueChange`. */

type BaseAccordionRootProps = ComponentPropsWithRef<typeof BaseAccordion.Root>;

interface AccordionSingleProps {
  /** Discriminator — `single` mode opens at most one Item at a time. */
  type: "single";
  /**
   * Controlled value — the `value` of the currently-open Item, or
   * `undefined` when nothing is open. Passing the prop (even as
   * `undefined`) puts the accordion into controlled mode; OMIT the
   * prop entirely to use `defaultValue` and stay uncontrolled. The
   * wrapper detects prop presence and forwards an empty selection
   * when `value === undefined`, so Base UI's `useControlled` semantics
   * never silently flip the accordion to uncontrolled.
   */
  value?: string;
  /**
   * Uncontrolled initial value — the `value` of the Item that should
   * be open on first mount. Pair with `onValueChange` to observe
   * changes without taking control.
   */
  defaultValue?: string;
  /**
   * Called when the open Item changes. Receives the new value, or
   * `undefined` if the only-open Item was closed in `collapsible`
   * mode.
   */
  onValueChange?: (value: string | undefined) => void;
  /**
   * When `true`, clicking the currently-open Trigger closes the Item
   * (so the accordion can be entirely collapsed). When `false`
   * (default), the open Item stays open until another Trigger is
   * clicked — RadioGroup semantics.
   *
   * @default false
   */
  collapsible?: boolean;
}

interface AccordionMultipleProps {
  /** Discriminator — `multiple` mode allows any number of Items open. */
  type: "multiple";
  /**
   * Controlled value — array of currently-open Item `value`s. Empty
   * array means nothing is open. Omit to leave uncontrolled and use
   * `defaultValue`.
   */
  value?: string[];
  /**
   * Uncontrolled initial value — array of Item `value`s that should
   * be open on first mount. Pair with `onValueChange` to observe.
   */
  defaultValue?: string[];
  /**
   * Called when the set of open Items changes. Receives the new
   * array of open values.
   */
  onValueChange?: (value: string[]) => void;
}

export type AccordionRootProps = (
  | AccordionSingleProps
  | AccordionMultipleProps
) & {
  /**
   * Visual orientation. `vertical` (default) stacks Items top-to-bottom
   * and uses Up/Down arrows for roving. `horizontal` lays Items
   * inline and uses Left/Right.
   *
   * @default "vertical"
   */
  orientation?: AccordionOrientation;
  /**
   * Whether the component should ignore user interaction. Cascades to
   * every Trigger inside.
   *
   * @default false
   */
  disabled?: boolean;
  /**
   * Whether to keep Panel content in the DOM while closed. Default
   * `false` unmounts closed panels (lighter DOM); set `true` when the
   * Panel state must survive a close/open cycle.
   *
   * @default false
   */
  keepMounted?: boolean;
  /** Optional class hook on the root container. */
  className?: string;
  /** Accordion children — `<Accordion.Item>` blocks. */
  children: ReactNode;
} & Omit<
    BaseAccordionRootProps,
    | "render"
    | "className"
    | "value"
    | "defaultValue"
    | "onValueChange"
    | "multiple"
    | "orientation"
    | "disabled"
    | "keepMounted"
    | "children"
  >;

function AccordionRootInner(
  props: AccordionRootProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const {
    orientation = "vertical",
    disabled = false,
    keepMounted = false,
    className,
    children,
    ...rest
  } = props;

  // Pull mode-specific props off the discriminator so we can map them
  // to Base UI's wire shape (which uses a single `multiple` boolean +
  // an array-typed value).
  if (rest.type === "single") {
    const {
      type: _type,
      onValueChange,
      collapsible = false,
      ...baseRest
    } = rest;
    void _type;
    // Detect controlled prop presence by inspecting the discriminator
    // shape itself — `value === undefined` cannot be used as the
    // signal because Base UI's `useControlled` treats `undefined` as
    // "uncontrolled". When the consumer explicitly passes the prop
    // (even as `undefined`), `'value' in rest` is true and we forward
    // an empty array so Base UI stays in controlled mode.
    const isControlled = "value" in rest;
    const isDefaulted = "defaultValue" in rest;
    const singleValue = isControlled
      ? (rest as { value?: string }).value
      : undefined;
    const singleDefaultValue = isDefaulted
      ? (rest as { defaultValue?: string }).defaultValue
      : undefined;
    // Base UI expects an ARRAY value internally. In single mode we
    // normalize to a one-element array (or empty when undefined) on
    // the way in, and back to a string (or undefined) on the way out.
    const baseValue = isControlled
      ? singleValue === undefined
        ? []
        : [singleValue]
      : undefined;
    const baseDefaultValue = isDefaulted
      ? singleDefaultValue === undefined
        ? []
        : [singleDefaultValue]
      : undefined;
    // Install Base UI's handler UNCONDITIONALLY for single mode so we
    // can call `eventDetails.cancel()` to enforce
    // `collapsible: false` — Base UI emits `[]` when the open
    // Trigger is re-clicked, and only an installed handler that
    // cancels can stop the closure.
    const baseOnValueChange = (
      next: unknown[],
      eventDetails: { cancel: () => void },
    ) => {
      const first = next.length > 0 ? String(next[0]) : undefined;
      // RadioGroup semantics: when collapsible is false the open
      // Trigger cannot close itself. Cancel the empty-array emit so
      // Base UI's internal state stays at the previous value.
      if (!collapsible && first === undefined) {
        eventDetails.cancel();
        return;
      }
      onValueChange?.(first);
    };
    return (
      <AccordionContext.Provider value={{ orientation }}>
        <BaseAccordion.Root
          {...(baseRest as object)}
          ref={ref}
          multiple={false}
          value={baseValue as never}
          defaultValue={baseDefaultValue as never}
          onValueChange={baseOnValueChange as never}
          orientation={orientation}
          disabled={disabled}
          keepMounted={keepMounted}
          className={classnames(
            "zs-accordion",
            `zs-accordion--${orientation}`,
            className,
          )}
          data-orientation={orientation}
        >
          {children}
        </BaseAccordion.Root>
      </AccordionContext.Provider>
    );
  }

  // Multiple mode. Pass arrays through unchanged.
  const {
    type: _type,
    onValueChange,
    ...baseRest
  } = rest;
  void _type;
  const isControlled = "value" in rest;
  const isDefaulted = "defaultValue" in rest;
  const multipleValue = isControlled
    ? (rest as { value?: string[] }).value
    : undefined;
  const multipleDefaultValue = isDefaulted
    ? (rest as { defaultValue?: string[] }).defaultValue
    : undefined;
  const baseOnValueChange = onValueChange
    ? (next: unknown[]) => {
        onValueChange(next.map((v) => String(v)));
      }
    : undefined;
  return (
    <AccordionContext.Provider value={{ orientation }}>
      <BaseAccordion.Root
        {...(baseRest as object)}
        ref={ref}
        multiple
        value={multipleValue as never}
        defaultValue={multipleDefaultValue as never}
        onValueChange={baseOnValueChange as never}
        orientation={orientation}
        disabled={disabled}
        keepMounted={keepMounted}
        className={classnames(
          "zs-accordion",
          `zs-accordion--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      >
        {children}
      </BaseAccordion.Root>
    </AccordionContext.Provider>
  );
}

/* ─── Accordion.Item ────────────────────────────────────────────────── */

type BaseAccordionItemProps = ComponentPropsWithRef<typeof BaseAccordion.Item>;

export interface AccordionItemProps
  extends Omit<BaseAccordionItemProps, "render" | "className" | "value"> {
  /**
   * The value keying this Item to the root's open-state. Required —
   * Items without a value cannot be addressed by `value`/`defaultValue`
   * on the root.
   */
  value: string;
  /** Optional class hook on the item container. */
  className?: string;
}

const AccordionItem = forwardRef<HTMLDivElement, AccordionItemProps>(
  function AccordionItem({ className, ...rest }, ref) {
    const { orientation } = useAccordionContext();
    return (
      <BaseAccordion.Item
        {...rest}
        ref={ref}
        className={classnames(
          "zs-accordion-item",
          `zs-accordion-item--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      />
    );
  },
);
(AccordionItem as { displayName?: string }).displayName = "Accordion.Item";

/* ─── Accordion.Header ──────────────────────────────────────────────── */

type BaseAccordionHeaderProps = ComponentPropsWithRef<
  typeof BaseAccordion.Header
>;

export interface AccordionHeaderProps
  extends Omit<BaseAccordionHeaderProps, "render" | "className"> {
  /** Optional class hook on the header element. */
  className?: string;
}

const AccordionHeader = forwardRef<HTMLHeadingElement, AccordionHeaderProps>(
  function AccordionHeader({ className, ...rest }, ref) {
    const { orientation } = useAccordionContext();
    return (
      <BaseAccordion.Header
        {...rest}
        ref={ref}
        className={classnames(
          "zs-accordion-header",
          `zs-accordion-header--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      />
    );
  },
);
(AccordionHeader as { displayName?: string }).displayName =
  "Accordion.Header";

/* ─── Accordion.Trigger ─────────────────────────────────────────────── */

type BaseAccordionTriggerProps = ComponentPropsWithRef<
  typeof BaseAccordion.Trigger
>;

export interface AccordionTriggerProps
  extends Omit<BaseAccordionTriggerProps, "render" | "className" | "type"> {
  /** Optional class hook on the trigger button. */
  className?: string;
}

const AccordionTrigger = forwardRef<HTMLButtonElement, AccordionTriggerProps>(
  function AccordionTrigger({ className, children, ...rest }, ref) {
    const { orientation } = useAccordionContext();
    return (
      <BaseAccordion.Trigger
        {...rest}
        ref={ref as React.Ref<HTMLElement>}
        // Stamp type="button" UNCONDITIONALLY. `type` is omitted from
        // AccordionTriggerProps so a caller can't override the stamp
        // by spreading `type="submit"` through `...rest`. Mirrors the
        // Tabs.Tab form-safety rule.
        type="button"
        className={classnames(
          "zs-accordion-trigger",
          `zs-accordion-trigger--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      >
        <span className="zs-accordion-trigger-label">{children}</span>
        <ChevronGlyph />
      </BaseAccordion.Trigger>
    );
  },
);
(AccordionTrigger as { displayName?: string }).displayName =
  "Accordion.Trigger";

/* ─── chevron glyph (inline so we don't pull a deps) ────────────────── */
function ChevronGlyph() {
  return (
    <svg
      className="zs-accordion-trigger-chevron"
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

/* ─── Accordion.Panel ───────────────────────────────────────────────── */

type BaseAccordionPanelProps = ComponentPropsWithRef<
  typeof BaseAccordion.Panel
>;

export interface AccordionPanelProps
  extends Omit<BaseAccordionPanelProps, "render" | "className"> {
  /** Optional class hook on the panel wrapper. */
  className?: string;
}

const AccordionPanel = forwardRef<HTMLDivElement, AccordionPanelProps>(
  function AccordionPanel({ className, children, ...rest }, ref) {
    const { orientation } = useAccordionContext();
    return (
      <BaseAccordion.Panel
        {...rest}
        ref={ref}
        className={classnames(
          "zs-accordion-panel",
          `zs-accordion-panel--${orientation}`,
          className,
        )}
        data-orientation={orientation}
      >
        {/* Inner wrapper carries the padding so the outer block-size
            animation can drive `block-size: 0` cleanly (padding on the
            outer would create a layout step at the transition end). */}
        <div className="zs-accordion-panel-inner">{children}</div>
      </BaseAccordion.Panel>
    );
  },
);
(AccordionPanel as { displayName?: string }).displayName = "Accordion.Panel";

/* ─── Compose the public namespace ──────────────────────────────────── */

const ForwardedAccordion = forwardRef(AccordionRootInner) as unknown as ((
  props: AccordionRootProps & { ref?: React.Ref<HTMLDivElement> },
) => React.JSX.Element) & {
  Root: typeof ForwardedAccordion;
  Item: typeof AccordionItem;
  Header: typeof AccordionHeader;
  Trigger: typeof AccordionTrigger;
  Panel: typeof AccordionPanel;
  displayName?: string;
};

(ForwardedAccordion as { displayName?: string }).displayName = "Accordion";
ForwardedAccordion.Root = ForwardedAccordion;
ForwardedAccordion.Item = AccordionItem;
ForwardedAccordion.Header = AccordionHeader;
ForwardedAccordion.Trigger = AccordionTrigger;
ForwardedAccordion.Panel = AccordionPanel;

export const Accordion = ForwardedAccordion;
export {
  AccordionItem,
  AccordionHeader,
  AccordionTrigger,
  AccordionPanel,
};
