import { type HTMLAttributes, type ReactNode } from "react";
import { Accordion as BaseAccordion } from "@base-ui/react/accordion";
import clsx from "clsx";

export interface AccordionItemOption {
  value: string;
  title: ReactNode;
  content: ReactNode;
  disabled?: boolean;
}

export interface AccordionProps extends HTMLAttributes<HTMLDivElement> {
  items: ReadonlyArray<AccordionItemOption>;
  value?: string[];
  defaultValue?: string[];
  onValueChange?: (value: string[]) => void;
  multiple?: boolean;
}

export function Accordion({
  items,
  value,
  defaultValue,
  onValueChange,
  multiple,
  className,
  ...props
}: AccordionProps) {
  return (
    <BaseAccordion.Root
      value={value}
      defaultValue={defaultValue}
      multiple={multiple}
      onValueChange={(next) => onValueChange?.(next.map(String))}
      className={clsx("zs-accordion", className)}
      {...props}
    >
      {items.map((item) => (
        <BaseAccordion.Item
          key={item.value}
          value={item.value}
          disabled={item.disabled}
          className="zs-accordion__item"
        >
          <BaseAccordion.Header className="zs-accordion__header">
            <BaseAccordion.Trigger className="zs-accordion__trigger">
              <span>{item.title}</span>
              <span className="zs-accordion__marker" aria-hidden="true" />
            </BaseAccordion.Trigger>
          </BaseAccordion.Header>
          <BaseAccordion.Panel className="zs-accordion__panel">
            {item.content}
          </BaseAccordion.Panel>
        </BaseAccordion.Item>
      ))}
    </BaseAccordion.Root>
  );
}

export const AccordionParts = BaseAccordion;
