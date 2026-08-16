import type { HTMLAttributes, ReactNode } from "react";

export function FieldError({
  as: Component = "p",
  children,
  className = "",
  role,
}: {
  as?: "p" | "span";
  children: ReactNode;
  className?: string;
  role?: HTMLAttributes<HTMLElement>["role"];
}) {
  return (
    <Component
      className={`field-error my-1 text-base text-danger ${className}`}
      role={role}
    >
      {children}
    </Component>
  );
}

export function Hint({
  as: Component = "p",
  children,
  className = "",
}: {
  as?: "p" | "span";
  children: ReactNode;
  className?: string;
}) {
  return (
    <Component className={`mb-2 text-md text-ink-muted ${className}`}>
      {children}
    </Component>
  );
}

/** Muted inline text. `dim` remains solely as the E2E locator hook. */
export function Muted({ children, className = "" }: { children: ReactNode; className?: string }) {
  return <span className={`dim text-ink-muted ${className}`}>{children}</span>;
}

export function InlineForm({
  children,
  className = "",
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={`relative mb-3 flex flex-wrap items-center gap-2 ${className}`}>
      {children}
    </div>
  );
}
