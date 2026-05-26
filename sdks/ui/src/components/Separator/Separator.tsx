import { forwardRef, type ComponentPropsWithoutRef } from "react";
import { Separator as BaseSeparator } from "@base-ui/react/separator";
import clsx from "clsx";

type BaseSeparatorProps = ComponentPropsWithoutRef<typeof BaseSeparator>;

export interface SeparatorProps extends Omit<BaseSeparatorProps, "className"> {
  className?: string;
}

export const Separator = forwardRef<HTMLDivElement, SeparatorProps>(
  function Separator({ className, orientation = "horizontal", ...props }, ref) {
    return (
      <BaseSeparator
        ref={ref}
        orientation={orientation}
        className={clsx("zs-separator", className)}
        {...props}
      />
    );
  },
);

export const SeparatorParts = BaseSeparator;
