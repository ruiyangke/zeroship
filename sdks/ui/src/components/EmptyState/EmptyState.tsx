import { type HTMLAttributes, type ReactNode } from "react";
import clsx from "clsx";

export interface EmptyStateProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  icon?: ReactNode;
}

export function EmptyState({
  title,
  description,
  action,
  icon,
  className,
  ...props
}: EmptyStateProps) {
  return (
    <div className={clsx("zs-empty", className)} {...props}>
      {icon && <div className="zs-empty__icon">{icon}</div>}
      <h2 className="zs-empty__title">{title}</h2>
      {description && <p className="zs-empty__description">{description}</p>}
      {action && <div className="zs-empty__action">{action}</div>}
    </div>
  );
}
