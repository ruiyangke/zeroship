import type { ReactNode } from "react";

export interface EmptyStateProps {
  title: string;
  description?: string;
  action?: ReactNode;
}

export function EmptyState({ title, description, action }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center justify-center py-12 px-6 text-center">
      <h3 className="font-display text-2xl font-medium text-ink mb-1">{title}</h3>
      {description && (
        <p className="font-serif text-sm text-ink-soft max-w-md mb-4">{description}</p>
      )}
      {action}
    </div>
  );
}
