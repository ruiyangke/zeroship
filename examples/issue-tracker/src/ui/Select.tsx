import { Select as BaseSelect } from "@base-ui/react/select";
import { forwardRef, type ReactNode } from "react";

export type SelectProps<Value = string> = Omit<
  BaseSelect.Root.Props<Value, false>,
  "children"
> & {
  children?: ReactNode;
  "aria-label"?: string;
  placeholder?: string;
  renderValue?: (value: Value) => ReactNode;
};

function ChevronDownIcon() {
  return (
    <svg
      aria-hidden="true"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  );
}

function CheckIcon() {
  return (
    <svg
      aria-hidden="true"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <path d="m5 12 4 4 10-10" />
    </svg>
  );
}

function SelectRoot<Value = string>(props: SelectProps<Value>) {
  const {
    "aria-label": ariaLabel,
    children,
    items,
    modal,
    placeholder,
    renderValue,
    value,
    ...rootProps
  } = props;
  const triggerLabel = ariaLabel ?? placeholder;

  return (
    <BaseSelect.Root
      {...rootProps}
      items={items}
      modal={modal ?? false}
      value={value}
    >
      <BaseSelect.Trigger
        {...(triggerLabel === undefined
          ? {}
          : { "aria-label": triggerLabel })}
        data-slot="select-trigger"
        data-size="md"
        data-variant="default"
      >
        <BaseSelect.Value
          data-slot="select-trigger-value"
          placeholder={placeholder}
        >
          {renderValue
            ? (current: unknown) =>
                current === null || current === undefined || current === ""
                  ? placeholder
                  : renderValue(current as Value)
            : items !== undefined && (value as unknown) === ""
              ? placeholder
              : undefined}
        </BaseSelect.Value>
        <BaseSelect.Icon data-slot="select-trigger-icon">
          <ChevronDownIcon />
        </BaseSelect.Icon>
      </BaseSelect.Trigger>

      <BaseSelect.Portal>
        <BaseSelect.Positioner
          align="start"
          alignItemWithTrigger={false}
          data-slot="select-positioner"
          side="bottom"
          sideOffset={6}
        >
          <BaseSelect.Popup
            data-size="md"
            data-slot="select-popup"
            style={{
              maxBlockSize: "var(--available-height)",
              minInlineSize: "var(--anchor-width)",
            }}
          >
            <BaseSelect.List data-slot="select-list">
              {children}
            </BaseSelect.List>
          </BaseSelect.Popup>
        </BaseSelect.Positioner>
      </BaseSelect.Portal>
    </BaseSelect.Root>
  );
}

const SelectItem = forwardRef<HTMLElement, BaseSelect.Item.Props>(
  function SelectItem({ children, ...props }, ref) {
    return (
      <BaseSelect.Item
        {...props}
        ref={ref}
        data-size="md"
        data-slot="select-item"
      >
        <BaseSelect.ItemIndicator
          data-slot="select-item-indicator"
          keepMounted
        >
          <CheckIcon />
        </BaseSelect.ItemIndicator>
        <BaseSelect.ItemText data-slot="select-item-text">
          {children}
        </BaseSelect.ItemText>
      </BaseSelect.Item>
    );
  },
);

SelectItem.displayName = "Select.Item";

export const Select = Object.assign(SelectRoot, { Item: SelectItem });
