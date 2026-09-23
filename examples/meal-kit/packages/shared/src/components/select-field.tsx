import {
  Children,
  isValidElement,
  type ComponentProps,
  type ReactNode,
} from "react";
import {
  Select as SelectRoot,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from "@gather/meal-kit/components/ui/select";
import { cn } from "@gather/meal-kit/lib/utils";

type SelectProps = Omit<
  ComponentProps<typeof SelectRoot<string>>,
  "onValueChange" | "items" | "multiple"
> & {
  onValueChange: (value: string) => void;
  className?: string;
  "aria-label"?: string;
  "aria-describedby"?: string;
  "aria-invalid"?: boolean;
};

export function Select({
  children,
  className,
  id,
  onValueChange,
  "aria-label": label,
  "aria-describedby": describedBy,
  "aria-invalid": invalid,
  ...props
}: SelectProps) {
  const items = Children.toArray(children)
    .filter(isValidElement<{ value: string; children: ReactNode }>)
    .map((item) => ({ value: item.props.value, label: item.props.children }));
  return (
    <SelectRoot
      {...props}
      items={items}
      onValueChange={(value) => {
        if (value !== null) onValueChange(value);
      }}
    >
      <SelectTrigger
        id={id}
        aria-label={label}
        aria-describedby={describedBy}
        aria-invalid={invalid}
        className={cn("w-full min-h-11", className)}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent align="start" alignItemWithTrigger={false}>
        {children}
      </SelectContent>
    </SelectRoot>
  );
}

export { SelectItem };
