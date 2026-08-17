import type { ComponentPropsWithoutRef, ElementType } from "react";

type AppFieldShellProps<T extends ElementType> = {
  as?: T;
  className?: string;
} & Omit<ComponentPropsWithoutRef<T>, "as" | "className">;

/**
 * Design-system field chrome for app-owned controls.
 *
 * The rich-text editor and native textarea use different controls from the
 * local Field wrapper, but they should share its edge and focus treatment.
 */
export function AppFieldShell<T extends ElementType = "div">({
  as,
  className = "",
  ...props
}: AppFieldShellProps<T>) {
  const Component = as ?? "div";

  return (
    <Component
      {...props}
      className={`app-field-shell field-edge rounded border-0 bg-surface text-ink transition duration-fast ease-out focus-within:bg-surface focus-within:field-edge-focus! ${className}`}
    />
  );
}
