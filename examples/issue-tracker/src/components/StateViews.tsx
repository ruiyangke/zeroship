// The three states every data view must render distinguishably: loading,
// empty, and error -- with a 401 (fail-closed "user" procedures hit while
// signed out) rendered as an explicit sign-in prompt, never as an empty list.
import type { ReactNode } from "react";
import { errorCode, errorMessage, isUnauthenticated } from "./rpc";

export function Loading({ label = "Loading..." }: { label?: string }) {
  return (
    <div className="state state-loading" role="status" aria-live="polite">
      <span className="spinner" aria-hidden="true" />
      <span>{label}</span>
    </div>
  );
}

export function EmptyState({
  title,
  hint,
}: {
  title: string;
  hint?: ReactNode;
}) {
  return (
    <div className="state state-empty">
      <p className="state-title">{title}</p>
      {hint ? <p className="state-hint">{hint}</p> : null}
    </div>
  );
}

export function ErrorState({
  error,
  onRetry,
}: {
  error: unknown;
  onRetry?: () => void;
}) {
  const authRequired = isUnauthenticated(error);
  return (
    <div className="state state-error" role="alert">
      <p className="state-title">
        {authRequired ? "Sign-in required" : "Something went wrong"}
      </p>
      <p className="state-hint">
        {authRequired
          ? "This needs a signed-in identity. Sign in through the platform and reload."
          : `${errorCode(error) ?? "ERROR"} · ${errorMessage(error)}`}
      </p>
      {onRetry ? (
        <button type="button" className="btn ghost small" onClick={onRetry}>
          {authRequired ? "Check again" : "Retry"}
        </button>
      ) : null}
    </div>
  );
}

/** Convenience wrapper for the common loading/error/empty/ready sequence. */
export function AsyncSection<T>({
  state,
  onRetry,
  loadingLabel,
  isEmpty,
  emptyTitle,
  emptyHint,
  children,
}: {
  state: { status: "loading" } | { status: "error"; error: unknown } | { status: "ready"; data: T };
  onRetry?: () => void;
  loadingLabel?: string;
  isEmpty?: (data: T) => boolean;
  emptyTitle?: string;
  emptyHint?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  if (state.status === "loading") return <Loading label={loadingLabel} />;
  if (state.status === "error") return <ErrorState error={state.error} onRetry={onRetry} />;
  if (isEmpty?.(state.data)) {
    return <EmptyState title={emptyTitle ?? "Nothing here yet"} hint={emptyHint} />;
  }
  return <>{children(state.data)}</>;
}
