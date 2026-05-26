import { type HTMLAttributes, type ReactNode } from "react";
import { Tabs as BaseTabs } from "@base-ui/react/tabs";
import clsx from "clsx";

export interface TabsItem {
  value: string;
  label: ReactNode;
  content: ReactNode;
  disabled?: boolean;
}

export interface TabsProps extends HTMLAttributes<HTMLDivElement> {
  items: TabsItem[];
  value?: string;
  defaultValue?: string;
  onValueChange?: (value: string) => void;
}

export function Tabs({
  items,
  value,
  defaultValue,
  onValueChange,
  className,
  ...props
}: TabsProps) {
  const firstEnabled = items.find((item) => !item.disabled)?.value ?? items[0]?.value;

  return (
    <BaseTabs.Root
      value={value}
      defaultValue={defaultValue ?? firstEnabled}
      onValueChange={(next) => onValueChange?.(String(next))}
      className={clsx("zs-tabs", className)}
      {...props}
    >
      <BaseTabs.List className="zs-tabs__list">
        {items.map((item) => (
          <BaseTabs.Tab
            key={item.value}
            value={item.value}
            disabled={item.disabled}
            className="zs-tabs__tab"
          >
            {item.label}
          </BaseTabs.Tab>
        ))}
      </BaseTabs.List>
      {items.map((item) => (
        <BaseTabs.Panel key={item.value} value={item.value} className="zs-tabs__panel">
          {item.content}
        </BaseTabs.Panel>
      ))}
    </BaseTabs.Root>
  );
}

export const TabsParts = BaseTabs;
