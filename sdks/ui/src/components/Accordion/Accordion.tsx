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
 *     `aria-expanded` and `aria-controls` wired to its Panel. We never
 *     attach a raw `onClick` that fights Base UI's controlled state —
 *     mode + value flow through the root. Disabled triggers are skipped
 *     by roving (ArrowDown/ArrowUp focus traversal).
 *
 *   - Accordion.Panel: a `<div>` carrying the matching `id` plus
 *     `aria-labelledby` pointing at its Trigger. Base UI animates the
 *     panel's block-size via the `--accordion-panel-height` CSS custom
 *     property; our CSS reads that var and transitions to/from `0`.
 *     `prefers-reduced-motion: reduce` snaps without a transition.
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
 *   2. Trigger is ALWAYS a `<button type="button">`. Stamping `type`
 *      defends against Accordion inside a Form (no accidental submit).
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
   * `undefined` when nothing is open. To leave the accordion
   * uncontrolled, omit this and use `defaultValue`.
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
      value,
      defaultValue,
      onValueChange,
      collapsible = false,
      ...baseRest
    } = rest;
    void _type;
    // Base UI expects an ARRAY value internally. In single mode we
    // normalize to a one-element array (or empty when undefined) on
    // the way in, and back to a string (or undefined) on the way out.
    const baseValue =
      value === undefined ? undefined : [value];
    const baseDefaultValue =
      defaultValue === undefined ? undefined : [defaultValue];
    const baseOnValueChange = onValueChange
      ? (next: unknown[]) => {
          // Single mode: collapsing returns []; else the only element.
          const first = next.length > 0 ? String(next[0]) : undefined;
          // When `collapsible` is false Base UI never emits an empty
          // array (the open item stays open), but defend defensively.
          if (!collapsible && first === undefined) return;
          onValueChange(first);
        }
      : undefined;
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
    value,
    defaultValue,
    onValueChange,
    ...baseRest
  } = rest;
  void _type;
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
        value={value as never}
        defaultValue={defaultValue as never}
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
  /**
   * The trigger's HTML `type`. Default `"button"` defends against an
   * accidental form submit when the accordion is inside a Form. Consumer
   * override still wins — but the default keeps Forms safe.
   *
   * @default "button"
   */
  type?: "button" | "submit" | "reset";
}

const AccordionTrigger = forwardRef<HTMLButtonElement, AccordionTriggerProps>(
  function AccordionTrigger({ className, type, children, ...rest }, ref) {
    const { orientation } = useAccordionContext();
    return (
      <BaseAccordion.Trigger
        {...rest}
        ref={ref as React.Ref<HTMLElement>}
        // Stamp type="button" defensively. Same defense Tabs/Toggle apply.
        type={type ?? "button"}
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
