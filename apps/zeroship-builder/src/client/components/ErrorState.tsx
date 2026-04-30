import type { ReactNode } from "react";
import { Button } from "./Button";

export interface ErrorStateProps {
  message: string;
  onRetry?: () => void;
  retryLabel?: string;
  detail?: ReactNode;
}

export function ErrorState({ message, onRetry, retryLabel = "Retry", detail }: ErrorStateProps) {
  return (
    <div role="alert" className="border border-blood/30 bg-blood/5 px-4 py-3 rounded">
      <div className="font-sans text-sm text-blood mb-1">{message}</div>
      {detail && <div className="font-mono text-[11px] text-ink-soft mb-2">{detail}</div>}
      {onRetry && (
        <Button variant="ghost" size="sm" onClick={onRetry}>
          {retryLabel}
        </Button>
      )}
    </div>
  );
}
