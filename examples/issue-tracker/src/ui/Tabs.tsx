import { Tabs as BaseTabs } from "@base-ui/react/tabs";
import {
  createContext,
  forwardRef,
  useContext,
  type ReactNode,
  type Ref,
} from "react";

export type TabsSize = "sm" | "md" | "lg";
export type TabsVariant = "default" | "pill" | "card";
export type TabsOrientation = "horizontal" | "vertical";

type TabsContextValue = {
  size: TabsSize;
  variant: TabsVariant;
  orientation: TabsOrientation;
  lazyMount: boolean;
};

const TabsContext = createContext<TabsContextValue>({
  size: "md",
  variant: "default",
  orientation: "horizontal",
  lazyMount: false,
});

export interface TabsProps extends Omit<
  BaseTabs.Root.Props,
  "className" | "orientation" | "render"
> {
  size?: TabsSize;
  variant?: TabsVariant;
  orientation?: TabsOrientation;
  lazyMount?: boolean;
  className?: string;
  children?: ReactNode;
}

const TabsRoot = forwardRef<HTMLDivElement, TabsProps>(function TabsRoot(
  {
    size = "md",
    variant = "default",
    orientation = "horizontal",
    lazyMount = false,
    className,
    children,
    ...props
  },
  ref,
) {
  return (
    <TabsContext.Provider value={{ size, variant, orientation, lazyMount }}>
      <BaseTabs.Root
        {...props}
        ref={ref}
        className={className}
        orientation={orientation}
        data-slot="tabs"
        data-size={size}
        data-variant={variant}
        data-orientation={orientation}
        data-lazy-mount={lazyMount ? "true" : "false"}
      >
        {children}
      </BaseTabs.Root>
    </TabsContext.Provider>
  );
});

export interface TabsListProps extends Omit<
  BaseTabs.List.Props,
  "className" | "render"
> {
  className?: string;
}

const TabsList = forwardRef<HTMLDivElement, TabsListProps>(function TabsList(
  { className, ...props },
  ref,
) {
  const { size, variant, orientation } = useContext(TabsContext);
  return (
    <BaseTabs.List
      {...props}
      ref={ref}
      className={className}
      data-slot="tabs-list"
      data-size={size}
      data-variant={variant}
      data-orientation={orientation}
    />
  );
});

export interface TabsTabProps extends Omit<
  BaseTabs.Tab.Props,
  "className" | "render" | "type" | "value"
> {
  value: string;
  size?: TabsSize;
  className?: string;
}

const TabsTab = forwardRef<HTMLButtonElement, TabsTabProps>(function TabsTab(
  { size: sizeProp, className, value, ...props },
  ref,
) {
  const { size: rootSize, variant, orientation } = useContext(TabsContext);
  return (
    <BaseTabs.Tab
      {...props}
      ref={ref as Ref<HTMLElement>}
      type="button"
      value={value}
      className={className}
      data-slot="tabs-tab"
      data-size={sizeProp ?? rootSize}
      data-variant={variant}
      data-orientation={orientation}
    />
  );
});

export interface TabsPanelProps extends Omit<
  BaseTabs.Panel.Props,
  "className" | "render" | "value"
> {
  value: string;
  className?: string;
}

const TabsPanel = forwardRef<HTMLDivElement, TabsPanelProps>(
  function TabsPanel({ className, keepMounted, value, ...props }, ref) {
    const { size, variant, orientation, lazyMount } = useContext(TabsContext);
    return (
      <BaseTabs.Panel
        {...props}
        ref={ref}
        value={value}
        keepMounted={keepMounted ?? !lazyMount}
        className={className}
        data-slot="tabs-panel"
        data-size={size}
        data-variant={variant}
        data-orientation={orientation}
      />
    );
  },
);

export interface TabsIndicatorProps extends Omit<
  BaseTabs.Indicator.Props,
  "className" | "render"
> {
  className?: string;
}

const TabsIndicator = forwardRef<HTMLSpanElement, TabsIndicatorProps>(
  function TabsIndicator({ className, ...props }, ref) {
    const { size, variant, orientation } = useContext(TabsContext);
    return (
      <BaseTabs.Indicator
        {...props}
        ref={ref}
        className={className}
        data-slot="tabs-indicator"
        data-size={size}
        data-variant={variant}
        data-orientation={orientation}
      />
    );
  },
);

TabsRoot.displayName = "Tabs";
TabsList.displayName = "Tabs.List";
TabsTab.displayName = "Tabs.Tab";
TabsPanel.displayName = "Tabs.Panel";
TabsIndicator.displayName = "Tabs.Indicator";

export const Tabs = Object.assign(TabsRoot, {
  List: TabsList,
  Tab: TabsTab,
  Panel: TabsPanel,
  Indicator: TabsIndicator,
});
