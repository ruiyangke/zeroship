/*
 * Tabs — TabList + Tab + Panel + Indicator anatomy.
 *
 * Wraps Base UI's `Tabs` primitive. The component reads as a way to swap
 * between sibling views inside a shared surface — not as a global nav (a
 * nav uses real anchors with focused-link semantics; tabs are
 * roving-tabindex'd buttons inside a tablist).
 *
 *   - Tabs (root): owns the controlled / uncontrolled value, orientation,
 *     and the variant / size cascade. Children declare their own anatomy
 *     via `<Tabs.List>`, `<Tabs.Tab>`, `<Tabs.Panel>`, `<Tabs.Indicator>`.
 *
 *   - Tabs.List: the role=tablist container. Roving tabindex + arrow-key
 *     navigation are handled by Base UI; we forward `activateOnFocus` and
 *     `loopFocus` so consumers can opt into "instant-activate on
 *     arrow-focus" vs the default "Enter/Space to activate".
 *
 *   - Tabs.Tab: a real `<button type="button">` carrying role=tab.
 *     `value` keys the tab to its panel. Disabled tabs are still
 *     ROVING-FOCUSABLE (Base UI hardcodes `disabledIndices: []` in its
 *     composite roving controller, so arrow keys land on a disabled
 *     tab as a focus stop) but cannot be ACTIVATED — Enter / Space /
 *     click on a disabled tab is a no-op (`aria-selected` does not
 *     flip; the panel does not swap). See Guarantee 10 for the
 *     verifiable contract.
 *
 *   - Tabs.Panel: role=tabpanel, auto-labelled by the matching Tab's id
 *     (Base UI wires `aria-labelledby` from its internal id registry).
 *     `lazyMount={true}` on the root replaces Base UI's default
 *     `keepMounted={true}` so inactive panels do NOT render — switching
 *     mounts them fresh.
 *
 *   - Tabs.Indicator: a `<span role="presentation">` that paints the
 *     animated active-tab affordance. Base UI computes the active tab's
 *     position and size and emits them as `--active-tab-left/top/width/
 *     height` CSS custom properties on the indicator element; our CSS
 *     reads those and transitions them. Reduced-motion suppresses the
 *     transition. The indicator is OPTIONAL — `default` variant ships
 *     with one by default; `pill` paints the active fill on the Tab
 *     button itself and consumers can omit the Indicator entirely (or
 *     leave it in and let it sit decoratively behind the pill).
 *
 * Design guarantees encoded here (in source so they travel with the code):
 *
 *   1. Three variants land on a clear ladder.
 *      `default` — underline indicator under the active tab. Quiet.
 *      `pill`    — accent-filled rounded segment under the active tab,
 *                  mirroring Toggle.Group's pressed pill (Slice 5).
 *      `card`    — each tab is an opaque raised surface; the active tab
 *                  visually merges into the panel surface below.
 *
 *   2. Horizontal + vertical orientation. Vertical groups stack the
 *      list to the side of the panels; the indicator translates
 *      vertically. The horizontal indicator anchors via physical
 *      `left:` so Base UI's physical `--active-tab-left` pixel value
 *      tracks the active tab in BOTH LTR and RTL. Logical properties
 *      handle the rail surface (gap, padding, border edges) so
 *      everything else flips automatically.
 *
 *   3. The Tab button is NEVER `submit` — `type="button"` is stamped
 *      unconditionally so a Tabs row inside a Form cannot accidentally
 *      submit it. The `type` prop is omitted from `TabsTabProps`
 *      entirely so a caller can't override the stamp. Same defense
 *      Toggle applies (Toggle is not a form control either).
 *
 *   4. Lazy mount: `lazyMount={true}` flips Base UI's `keepMounted`
 *      default to `false`. Consumers who need preserved state across
 *      switches leave the prop off; consumers with expensive panels
 *      opt in.
 *
 *   5. The size cascade follows the Field / Radio / Toggle pattern:
 *      explicit prop on Tab > root context > default `md`. A nested
 *      Tab can override the root size, but typically a Tabs row picks
 *      one size for the whole list.
 *
 *   6. The variant cascade is one-way: the root sets the variant, every
 *      Tab inherits it. Per-Tab variant overrides aren't supported —
 *      mixed variants inside one list read incoherent.
 *
 *   7. The public API is the `Tabs` namespace ONLY — `Tabs.List`,
 *      `Tabs.Tab`, `Tabs.Panel`, `Tabs.Indicator`. No bare
 *      `TabsList`/`TabsTab`/... exports; one obvious path per the
 *      AI-friendly API guidelines.
 *
 *   8. `prefers-reduced-motion`: CSS suppresses the indicator transition.
 *
 *   9. Tab values are STRING-KEYED. We don't carry a `<Value extends
 *      string>` generic on the root + children because Base UI types
 *      `TabsTab.Value` as `any`, so a generic narrowing on our shell
 *      would be decorative — the controlled value callback would still
 *      arrive as `any` from Base UI. Consumers pick whatever string
 *      keys they like ("overview", "billing", "1", ...) and read them
 *      back as strings.
 *
 *  10. Disabled-tab keyboard contract: a disabled Tab remains in the
 *      roving order (Base UI's composite controller hardcodes
 *      `disabledIndices: []`), so ArrowRight CAN focus it. The tab
 *      cannot be ACTIVATED — Enter/Space/click on a focused disabled
 *      tab does not flip `aria-selected` and does not swap the panel.
 *      If a future Base UI release exposes a "skip disabled" knob, we
 *      should flip the contract to "disabled tabs are skipped by
 *      roving" — until then, the honest behavior is documented here.
 */
import {
  createContext,
  forwardRef,
  useContext,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import { Tabs as BaseTabs } from "@base-ui/react/tabs";
import { classnames } from "../_classnames";

export type TabsSize = "sm" | "md" | "lg";
export type TabsVariant = "default" | "pill" | "card";
export type TabsOrientation = "horizontal" | "vertical";

/* ─── root-local context ────────────────────────────────────────────── *
 *
 * Carries the root's `size` and `variant` so children pick them up
 * without climbing the React tree manually. Explicit props on a child
 * still win over context (size only — variant is one-way per guarantee
 * 6). Orientation is carried so List/Indicator/Panel CSS can scope to
 * it via data attributes without re-reading Base UI's state. */
interface TabsContextValue {
  size: TabsSize;
  variant: TabsVariant;
  orientation: TabsOrientation;
}

const TabsContext = createContext<TabsContextValue | null>(null);

function useTabsContext(): TabsContextValue {
  return (
    useContext(TabsContext) ?? {
      size: "md",
      variant: "default",
      orientation: "horizontal",
    }
  );
}

/* ─── Tabs root ─────────────────────────────────────────────────────── */

type BaseTabsRootProps = ComponentPropsWithRef<typeof BaseTabs.Root>;

export interface TabsProps
  extends Omit<BaseTabsRootProps, "render" | "className"> {
  /**
   * Visual variant — `default` paints an underline indicator under the
   * active tab; `pill` paints an accent-filled rounded segment behind
   * the active tab (Toggle.Group's pressed pill pattern, Slice 5);
   * `card` raises each tab as an opaque surface that visually merges
   * into the panel on activation.
   *
   * @default "default"
   */
  variant?: TabsVariant;
  /**
   * Visual size — sm / md (default) / lg. Mirrors Button + Input
   * rhythm so a Tabs row next to either reads coherent.
   *
   * @default "md"
   */
  size?: TabsSize;
  /**
   * Layout orientation. `horizontal` lays the tablist row above the
   * panels; `vertical` lays the tablist as a column beside the panels
   * and rotates the indicator. Drives `aria-orientation` on the
   * tablist so screen readers announce the correct arrow-key axis.
   *
   * @default "horizontal"
   */
  orientation?: TabsOrientation;
  /**
   * Lazy-mount panel content. When `true`, only the active Panel
   * renders to the DOM; switching unmounts the previous and mounts the
   * next. Default `false` matches Base UI's `keepMounted: true` so all
   * Panel content stays mounted (state is preserved across switches).
   *
   * Set to `true` when panel content is expensive (heavy charts, large
   * tables) and the cost of remount on switch is cheaper than the cost
   * of keeping every panel hydrated.
   *
   * @default false
   */
  lazyMount?: boolean;
  /** Optional class hook on the root container. */
  className?: string;
  /** Tabs children — List, Indicator, Panel(s). */
  children?: ReactNode;
}

function TabsRootInner(
  props: TabsProps,
  ref: React.ForwardedRef<HTMLDivElement>,
) {
  const {
    variant = "default",
    size = "md",
    orientation = "horizontal",
    lazyMount = false,
    className,
    children,
    ...rest
  } = props;

  return (
    <TabsContext.Provider value={{ size, variant, orientation }}>
      <BaseTabs.Root
        {...rest}
        ref={ref}
        orientation={orientation}
        className={classnames(
          "zs-tabs",
          `zs-tabs--${variant}`,
          `zs-tabs--${size}`,
          `zs-tabs--${orientation}`,
          className,
        )}
        data-variant={variant}
        data-size={size}
        data-orientation={orientation}
        data-lazy-mount={lazyMount ? "true" : "false"}
      >
        <LazyMountContext.Provider value={lazyMount}>
          {children}
        </LazyMountContext.Provider>
      </BaseTabs.Root>
    </TabsContext.Provider>
  );
}

const LazyMountContext = createContext<boolean>(false);

/* ─── Tabs.List ─────────────────────────────────────────────────────── */

type BaseTabsListProps = ComponentPropsWithRef<typeof BaseTabs.List>;

export interface TabsListProps
  extends Omit<BaseTabsListProps, "render" | "className" | "activateOnFocus" | "loopFocus"> {
  /**
   * When `true`, focusing a Tab via arrow-key navigation activates it
   * immediately (selects the tab and shows its panel). When `false`
   * (the default), arrow keys only ROVE focus; the consumer presses
   * Enter or Space to activate. Both shapes are valid per WAI-ARIA
   * APG; pick `true` for "swap on focus" UX (typically for read-only
   * dashboards) and leave `false` for two-step activation.
   *
   * @default false
   */
  activateOnFocus?: boolean;
  /**
   * Loop arrow-key focus from the last tab back to the first (and
   * vice versa). When `false`, ArrowRight on the last tab is a no-op.
   *
   * @default true
   */
  loopFocus?: boolean;
  /** Optional class hook on the list container. */
  className?: string;
}

const TabsList = forwardRef<HTMLDivElement, TabsListProps>(function TabsList(
  { className, ...rest },
  ref,
) {
  const { variant, size, orientation } = useTabsContext();
  return (
    <BaseTabs.List
      {...rest}
      ref={ref}
      className={classnames(
        "zs-tabs-list",
        `zs-tabs-list--${variant}`,
        `zs-tabs-list--${size}`,
        `zs-tabs-list--${orientation}`,
        className,
      )}
      data-variant={variant}
      data-size={size}
      data-orientation={orientation}
    />
  );
});
(TabsList as { displayName?: string }).displayName = "Tabs.List";

/* ─── Tabs.Tab ──────────────────────────────────────────────────────── */

type BaseTabsTabProps = ComponentPropsWithRef<typeof BaseTabs.Tab>;

export interface TabsTabProps
  extends Omit<BaseTabsTabProps, "render" | "className" | "value" | "type"> {
  /**
   * The value keying this Tab to its Panel. Required — there is no
   * implicit index-based wiring; Panels reference Tabs by `value`.
   * Values are string-keyed; see Guarantee 9 in the header comment.
   */
  value: string;
  /**
   * Whether the Tab is disabled. A disabled Tab cannot be ACTIVATED —
   * click and Enter/Space are no-ops, `aria-selected` does not flip,
   * and the panel does not swap. It IS still in the roving-tab order
   * (Base UI hardcodes `disabledIndices: []`), so ArrowLeft / Right
   * can park focus on it; the focus stop is announced but no
   * activation can follow. See Guarantee 10 in the header comment.
   *
   * @default false
   */
  disabled?: boolean;
  /** Optional size override. Inherits root context when omitted. */
  size?: TabsSize;
  /** Optional class hook on the tab button. */
  className?: string;
}

const TabsTab = forwardRef<HTMLButtonElement, TabsTabProps>(function TabsTab(
  { value, size: sizeProp, className, ...rest },
  ref,
) {
  const ctx = useTabsContext();
  // Explicit prop wins over root context. Mirrors the Toggle / Radio /
  // Field cascade.
  const size = sizeProp ?? ctx.size;
  return (
    <BaseTabs.Tab
      {...rest}
      ref={ref}
      value={value}
      // Stamp type="button" UNCONDITIONALLY so a Tabs row inside a Form
      // cannot accidentally submit it. `type` is omitted from
      // TabsTabProps so a caller can't override the stamp by spreading
      // a `type="submit"` through `...rest`.
      type="button"
      className={classnames(
        "zs-tabs-tab",
        `zs-tabs-tab--${ctx.variant}`,
        `zs-tabs-tab--${size}`,
        `zs-tabs-tab--${ctx.orientation}`,
        className,
      )}
      data-variant={ctx.variant}
      data-size={size}
      data-orientation={ctx.orientation}
    />
  );
});
(TabsTab as { displayName?: string }).displayName = "Tabs.Tab";

/* ─── Tabs.Panel ────────────────────────────────────────────────────── */

type BaseTabsPanelProps = ComponentPropsWithRef<typeof BaseTabs.Panel>;

export interface TabsPanelProps
  extends Omit<BaseTabsPanelProps, "render" | "className" | "value" | "keepMounted"> {
  /**
   * The value of the Tab this Panel pairs with. Required. String-keyed
   * (see Guarantee 9 in the header comment).
   */
  value: string;
  /**
   * Per-Panel override of the root's `lazyMount`. Most consumers leave
   * this off and let the root decide; a one-off expensive Panel inside
   * a default-mounted Tabs row can opt out individually by setting
   * `keepMounted={false}`.
   */
  keepMounted?: boolean;
  /** Optional class hook on the panel wrapper. */
  className?: string;
}

const TabsPanel = forwardRef<HTMLDivElement, TabsPanelProps>(function TabsPanel(
  { value, keepMounted, className, ...rest },
  ref,
) {
  const ctx = useTabsContext();
  const lazyRoot = useContext(LazyMountContext);
  // Per-Panel override wins; otherwise invert the root's lazyMount to
  // Base UI's keepMounted contract.
  const resolvedKeepMounted = keepMounted ?? !lazyRoot;
  return (
    <BaseTabs.Panel
      {...rest}
      ref={ref}
      value={value}
      keepMounted={resolvedKeepMounted}
      className={classnames(
        "zs-tabs-panel",
        `zs-tabs-panel--${ctx.variant}`,
        `zs-tabs-panel--${ctx.size}`,
        `zs-tabs-panel--${ctx.orientation}`,
        className,
      )}
      data-variant={ctx.variant}
      data-size={ctx.size}
      data-orientation={ctx.orientation}
    />
  );
});
(TabsPanel as { displayName?: string }).displayName = "Tabs.Panel";

/* ─── Tabs.Indicator ────────────────────────────────────────────────── */

type BaseTabsIndicatorProps = ComponentPropsWithRef<typeof BaseTabs.Indicator>;

export interface TabsIndicatorProps
  extends Omit<BaseTabsIndicatorProps, "render" | "className"> {
  /** Optional class hook on the indicator span. */
  className?: string;
}

const TabsIndicator = forwardRef<HTMLSpanElement, TabsIndicatorProps>(
  function TabsIndicator({ className, ...rest }, ref) {
    const { variant, size, orientation } = useTabsContext();
    return (
      <BaseTabs.Indicator
        {...rest}
        ref={ref}
        className={classnames(
          "zs-tabs-indicator",
          `zs-tabs-indicator--${variant}`,
          `zs-tabs-indicator--${size}`,
          `zs-tabs-indicator--${orientation}`,
          className,
        )}
        data-variant={variant}
        data-size={size}
        data-orientation={orientation}
      />
    );
  },
);
(TabsIndicator as { displayName?: string }).displayName = "Tabs.Indicator";

/* ─── Compose the public namespace ──────────────────────────────────── *
 *
 * Public API surface = the `Tabs` namespace only (Guarantee 7). The
 * subpart components are not exported as bare symbols — `Tabs.List`,
 * `Tabs.Tab`, `Tabs.Panel`, `Tabs.Indicator` is the one obvious path. */

const ForwardedTabs = forwardRef<HTMLDivElement, TabsProps>(TabsRootInner) as
  React.ForwardRefExoticComponent<TabsProps & React.RefAttributes<HTMLDivElement>>
  & {
    List: typeof TabsList;
    Tab: typeof TabsTab;
    Panel: typeof TabsPanel;
    Indicator: typeof TabsIndicator;
  };

ForwardedTabs.displayName = "Tabs";
ForwardedTabs.List = TabsList;
ForwardedTabs.Tab = TabsTab;
ForwardedTabs.Panel = TabsPanel;
ForwardedTabs.Indicator = TabsIndicator;

export const Tabs = ForwardedTabs;
