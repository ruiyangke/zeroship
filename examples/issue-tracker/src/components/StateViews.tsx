// The three states every data view must render distinguishably: loading,
// empty, and error -- with a 401 (fail-closed "user" procedures hit while
// signed out) rendered as an explicit sign-in prompt, never as an empty list.
import type { ReactNode } from "react";
import {
  Button,
  EmptyState as UiEmptyState,
  ErrorState as UiErrorState,
  Spinner,
  Stack,
} from "@zeroship/ui";

import { errorCode, errorMessage, isUnauthenticated, type AsyncState } from "./rpc";

export function Loading({ label = "Loading..." }: { label?: string }) {
  return (
    <Stack className="state state-loading" gap={2} align="center" role="status" aria-live="polite">
      <Spinner />
      <span>{label}</span>
    </Stack>
  );
}

export function EmptyState({
  title,
  hint,
  tone = "block",
}: {
  title: string;
  hint?: ReactNode;
  /**
   * "block" is the page-level empty state: a centred illustration-scale
   * heading, right when a whole list has nothing in it.
   *
   * "inline" is for a panel. The bug page carries nine small panels side by
   * side, and most of them are empty on most bugs -- no keywords, no votes,
   * no dependencies. Rendering "No keywords have been defined for this
   * tracker yet." as a centred heading inside a 280px card made an ordinary
   * state look like a failure, nine times over. Here the empty case is an
   * aside, so it says its piece in one quiet line.
   */
  tone?: "block" | "inline";
}) {
  if (tone === "inline") {
    return (
      <p className="state-hint small">
        {title}
        {hint ? <> {hint}</> : null}
      </p>
    );
  }
  // Ergonomic props only. The block composes rather than suppresses, so
  // passing a title prop AND a <Title> child renders two headings.
  return <UiEmptyState title={title} description={hint} />;
}

export function ErrorState({
  error,
  onRetry,
}: {
  error: unknown;
  onRetry?: () => void;
}) {
  // A 401 is not a failure, it is a state: the fail-closed procedures return
  // it whenever a signed-out visitor reaches one. It renders as a WARNING with
  // an instruction, not a red error -- nothing is broken and there is nothing
  // to retry until they sign in.
  const authRequired = isUnauthenticated(error);
  return (
    <UiErrorState
      intent={authRequired ? "warning" : "danger"}
      title={authRequired ? "Sign-in required" : "Something went wrong"}
      description={
        authRequired
          ? "This needs a signed-in identity. Sign in through the platform and reload."
          : `${errorCode(error) ?? "ERROR"} · ${errorMessage(error)}`
      }
    >
      {/* The action is a compound child rather than the ergonomic onRetry
          prop, whose button is labelled "Retry" and cannot be changed. There
          is nothing to retry when the answer is 401 -- the visitor has to sign
          in first, so the button says what it does. */}
      {onRetry ? (
        <UiErrorState.Actions>
          <Button variant="filled" onClick={onRetry}>
            {authRequired ? "Check again" : "Retry"}
          </Button>
        </UiErrorState.Actions>
      ) : null}
    </UiErrorState>
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
  emptyTone,
  renderLoading,
  children,
}: {
  // The shared AsyncState, not a re-spelling of it. This was an inline copy
  // of the same union, so adding `refreshing` to the real one left this
  // signature quietly behind.
  state: AsyncState<T>;
  onRetry?: () => void;
  loadingLabel?: string;
  isEmpty?: (data: T) => boolean;
  emptyTitle?: string;
  emptyHint?: ReactNode;
  /** Passed through to EmptyState -- "inline" for small side panels. */
  emptyTone?: "block" | "inline";
  /**
   * What to show while the FIRST load is in flight, instead of a spinner.
   *
   * A table should be marked as loading, not replaced by one: swapping the
   * whole surface for a spinner throws away the column headers, the filter
   * context and the page height, so the layout jumps when data lands and you
   * lose the thing you were looking at. A caller that can render its own
   * skeleton passes it here.
   */
  renderLoading?: () => ReactNode;
  children: (data: T, refreshing: boolean) => ReactNode;
}) {
  if (state.status === "loading") return renderLoading ? <>{renderLoading()}</> : <Loading label={loadingLabel} />;
  if (state.status === "error") return <ErrorState error={state.error} onRetry={onRetry} />;
  if (isEmpty?.(state.data)) {
    return (
      <EmptyState title={emptyTitle ?? "Nothing here yet"} hint={emptyHint} tone={emptyTone} />
    );
  }
  // The refreshing flag reaches the child rather than being swallowed here.
  // A section that knows it is reloading can mark itself busy in place; the
  // alternative is unmounting it, which is what this component used to do.
  return <>{children(state.data, state.refreshing === true)}</>;
}
