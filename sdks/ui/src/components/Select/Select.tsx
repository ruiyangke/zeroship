import {
  Children,
  forwardRef,
  type ButtonHTMLAttributes,
  isValidElement,
  type ChangeEventHandler,
  type ReactElement,
  type ReactNode,
} from "react";
import { Select as BaseSelect } from "@base-ui/react/select";
import clsx from "clsx";
import { FieldFrame } from "../Field";

export interface SelectItemOption {
  value: string;
  label: ReactNode;
  disabled?: boolean;
}

export interface SelectProps
  extends Omit<
    ButtonHTMLAttributes<HTMLButtonElement>,
    "children" | "value" | "defaultValue" | "onChange"
  > {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  items?: ReadonlyArray<SelectItemOption> | Record<string, ReactNode>;
  children?: ReactNode;
  value?: string | null;
  defaultValue?: string | null;
  onValueChange?: (value: string | null) => void;
  onChange?: ChangeEventHandler<HTMLSelectElement>;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  placeholder?: ReactNode;
  readOnly?: boolean;
  required?: boolean;
  autoComplete?: string;
}

function optionsFromItems(
  items: SelectProps["items"],
  children: ReactNode,
): SelectItemOption[] {
  if (Array.isArray(items)) return [...items];
  if (items && typeof items === "object") {
    return Object.entries(items).map(([value, label]) => ({ value, label }));
  }

  const options: SelectItemOption[] = [];
  Children.forEach(children, (child) => {
    if (!isValidElement(child)) return;
    const option = child as ReactElement<{
      value?: string | number;
      disabled?: boolean;
      children?: ReactNode;
    }>;
    if (option.type !== "option") return;
    options.push({
      value: String(option.props.value ?? ""),
      label: option.props.children,
      disabled: option.props.disabled,
    });
  });
  return options;
}

function dispatchNativeChange(
  onChange: ChangeEventHandler<HTMLSelectElement> | undefined,
  name: string | undefined,
  value: string | null,
) {
  if (!onChange) return;
  onChange({
    target: { name, value: value ?? "" },
    currentTarget: { name, value: value ?? "" },
  } as unknown as Parameters<ChangeEventHandler<HTMLSelectElement>>[0]);
}

export const Select = forwardRef<HTMLButtonElement, SelectProps>(function Select(
  {
    label,
    hint,
    error,
    items,
    children,
    className,
    id,
    name,
    value,
    defaultValue,
    onValueChange,
    onChange,
    open,
    defaultOpen,
    onOpenChange,
    placeholder = "Select an option",
    disabled,
    required,
    readOnly,
    form,
    autoComplete,
    ...props
  },
  ref,
) {
  const options = optionsFromItems(items, children);

  return (
    <FieldFrame
      label={label}
      hint={hint}
      error={error}
      disabled={disabled}
      labelNative={false}
    >
      <BaseSelect.Root
        id={id}
        name={name}
        value={value}
        defaultValue={defaultValue}
        open={open}
        defaultOpen={defaultOpen}
        onOpenChange={(next) => onOpenChange?.(next)}
        disabled={disabled}
        required={required}
        readOnly={readOnly}
        form={form}
        autoComplete={autoComplete}
        items={options}
        onValueChange={(next) => {
          const stringValue = next == null ? null : String(next);
          onValueChange?.(stringValue);
          dispatchNativeChange(onChange, name, stringValue);
        }}
      >
        <BaseSelect.Trigger
          ref={ref}
          className={clsx("zs-select", className)}
          aria-invalid={error ? true : undefined}
          {...props}
        >
          <BaseSelect.Value placeholder={placeholder} />
          <BaseSelect.Icon className="zs-select__icon">
            <svg viewBox="0 0 12 12" width="12" height="12" fill="none" aria-hidden="true">
              <path
                d="M3 4.5 6 7.5l3-3"
                stroke="currentColor"
                strokeWidth="1.5"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </BaseSelect.Icon>
        </BaseSelect.Trigger>
        <BaseSelect.Portal>
          <BaseSelect.Positioner
            alignItemWithTrigger={false}
            className="zs-select__positioner"
            sideOffset={4}
          >
            <BaseSelect.Popup className="zs-select__popup">
              <BaseSelect.List className="zs-select__list">
                {options.map((item) => (
                  <BaseSelect.Item
                    key={item.value}
                    value={item.value}
                    disabled={item.disabled}
                    className="zs-select__item"
                  >
                    <BaseSelect.ItemIndicator className="zs-select__item-indicator">
                      <svg viewBox="0 0 12 12" width="12" height="12" fill="none" aria-hidden="true">
                        <path
                          d="M2.5 6.5 5 9l4.5-5.5"
                          stroke="currentColor"
                          strokeWidth="1.5"
                          strokeLinecap="round"
                          strokeLinejoin="round"
                        />
                      </svg>
                    </BaseSelect.ItemIndicator>
                    <BaseSelect.ItemText>{item.label}</BaseSelect.ItemText>
                  </BaseSelect.Item>
                ))}
              </BaseSelect.List>
            </BaseSelect.Popup>
          </BaseSelect.Positioner>
        </BaseSelect.Portal>
      </BaseSelect.Root>
    </FieldFrame>
  );
});

export const SelectParts = BaseSelect;
