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
      className={`app-field-shell border-0 bg-[var(--zeroship-input-bg)] text-[var(--zeroship-input-ink)] [box-shadow:var(--zeroship-input-surface-edge,_inset_0_0_0_0_transparent),_inset_0_0_0_var(--zeroship-input-border-width,_0.0625rem)_var(--zeroship-input-border),_0_0_0_0_transparent] transition-[background-color,box-shadow] duration-[var(--zeroship-motion-fast)] ease-[var(--zeroship-motion-ease)] rounded-[var(--zeroship-control-radius-md)] focus-within:bg-[var(--zeroship-input-bg-focus)] focus-within:[box-shadow:var(--zeroship-input-surface-edge,_inset_0_0_0_0_transparent),_inset_0_0_0_var(--zeroship-input-border-width,_0.0625rem)_var(--zeroship-input-border-focus),_0_0_0_var(--zeroship-focus-ring-width)_var(--zeroship-input-focus-ring-color)]! ${className}`}
    />
  );
}
