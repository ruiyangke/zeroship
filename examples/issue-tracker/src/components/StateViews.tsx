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
}: {
  title: string;
  hint?: ReactNode;
}) {
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
  children: (data: T, refreshing: boolean) => ReactNode;
}) {
  if (state.status === "loading") return <Loading label={loadingLabel} />;
  if (state.status === "error") return <ErrorState error={state.error} onRetry={onRetry} />;
  if (isEmpty?.(state.data)) {
    return <EmptyState title={emptyTitle ?? "Nothing here yet"} hint={emptyHint} />;
  }
  // The refreshing flag reaches the child rather than being swallowed here.
  // A section that knows it is reloading can mark itself busy in place; the
  // alternative is unmounting it, which is what this component used to do.
  return <>{children(state.data, state.refreshing === true)}</>;
}
