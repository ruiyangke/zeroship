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
 *     `value` keys the tab to its panel. Disabled tabs are skipped by
 *     roving navigation.
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
 *      vertically. Logical properties keep RTL automatic.
 *
 *   3. The Tab button is NEVER `submit` — `type="button"` is stamped so
 *      a Tabs row inside a Form doesn't accidentally submit it. Same
 *      defense Toggle applies (Toggle is not a form control either).
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
 *   7. No `Tabs.Indicator` re-export from the package root is necessary —
 *      it ships on the `Tabs` namespace as `Tabs.Indicator`. Same
 *      shape Toggle.Group uses.
 *
 *   8. `prefers-reduced-motion`: CSS suppresses the indicator transition.
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

export interface TabsProps<Value extends string = string>
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

function TabsRootInner<Value extends string = string>(
  props: TabsProps<Value>,
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

export interface TabsListProps extends Omit<BaseTabsListProps, "render" | "className"> {
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
(TabsList as React.FC).displayName = "Tabs.List";

/* ─── Tabs.Tab ──────────────────────────────────────────────────────── */

type BaseTabsTabProps = ComponentPropsWithRef<typeof BaseTabs.Tab>;

export interface TabsTabProps<Value extends string = string>
  extends Omit<BaseTabsTabProps, "render" | "className" | "value"> {
  /**
   * The value keying this Tab to its Panel. Required — there is no
   * implicit index-based wiring; Panels reference Tabs by `value`.
   */
  value: Value;
  /** Optional size override. Inherits root context when omitted. */
  size?: TabsSize;
  /** Optional class hook on the tab button. */
  className?: string;
}

const TabsTab = forwardRef(function TabsTab<Value extends string = string>(
  {
    value,
    size: sizeProp,
    className,
    type,
    ...rest
  }: TabsTabProps<Value>,
  ref: React.ForwardedRef<HTMLButtonElement>,
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
      // Stamp type="button" defensively so a Tabs row inside a Form
      // doesn't accidentally submit. Consumer-supplied `type` still wins
      // (e.g. for an asChild `<a>` swap a future revision might offer).
      type={type ?? "button"}
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
}) as <Value extends string = string>(
  props: TabsTabProps<Value> & { ref?: React.Ref<HTMLButtonElement> },
) => React.JSX.Element;
(TabsTab as { displayName?: string }).displayName = "Tabs.Tab";

/* ─── Tabs.Panel ────────────────────────────────────────────────────── */

type BaseTabsPanelProps = ComponentPropsWithRef<typeof BaseTabs.Panel>;

export interface TabsPanelProps<Value extends string = string>
  extends Omit<BaseTabsPanelProps, "render" | "className" | "value" | "keepMounted"> {
  /** The value of the Tab this Panel pairs with. Required. */
  value: Value;
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

const TabsPanel = forwardRef(function TabsPanel<Value extends string = string>(
  { value, keepMounted, className, ...rest }: TabsPanelProps<Value>,
  ref: React.ForwardedRef<HTMLDivElement>,
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
}) as <Value extends string = string>(
  props: TabsPanelProps<Value> & { ref?: React.Ref<HTMLDivElement> },
) => React.JSX.Element;
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
(TabsIndicator as React.FC).displayName = "Tabs.Indicator";

/* ─── Compose the public namespace ──────────────────────────────────── */

const ForwardedTabs = forwardRef(TabsRootInner) as unknown as (<
  Value extends string = string,
>(
  props: TabsProps<Value> & { ref?: React.Ref<HTMLDivElement> },
) => React.JSX.Element) & {
  List: typeof TabsList;
  Tab: typeof TabsTab;
  Panel: typeof TabsPanel;
  Indicator: typeof TabsIndicator;
  displayName?: string;
};

(ForwardedTabs as { displayName?: string }).displayName = "Tabs";
ForwardedTabs.List = TabsList;
ForwardedTabs.Tab = TabsTab;
ForwardedTabs.Panel = TabsPanel;
ForwardedTabs.Indicator = TabsIndicator;

export const Tabs = ForwardedTabs;
export { TabsList, TabsTab, TabsPanel, TabsIndicator };
