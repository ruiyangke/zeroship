// The three states every data view must render distinguishably: loading,
// empty, and error -- with a 401 (fail-closed "user" procedures hit while
// signed out) rendered as an explicit sign-in prompt, never as an empty list.
import type { ReactNode } from "react";
import { EmptyState as UiEmptyState } from "../ui/EmptyState";
import { ErrorState as UiErrorState } from "../ui/ErrorState";
import { Button } from "../ui/Button";
import { Spinner } from "../ui/Spinner";

import { errorCode, errorMessage, isUnauthenticated } from "./rpc";
import { Hint } from "./AppPrimitives";

export function Loading({ label = "Loading..." }: { label?: string }) {
  return (
    <div
      className="flex min-w-0 flex-col flex-nowrap items-center justify-start gap-2 px-2 py-6 text-ink-secondary [&>*]:min-h-0 [&>*]:min-w-0"
      role="status"
      aria-live="polite"
    >
      <Spinner />
      <span>{label}</span>
    </div>
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
   * "inline" is for a panel. The issue page carries nine small panels side by
   * side, and most of them are empty on most issues -- no keywords, no votes,
   * no dependencies. Rendering "No keywords have been defined for this
   * tracker yet." as a centred heading inside a 280px card made an ordinary
   * state look like a failure, nine times over. Here the empty case is an
   * aside, so it says its piece in one quiet line.
   */
  tone?: "block" | "inline";
}) {
  if (tone === "inline") {
    return (
      <Hint>
        {title}
        {hint ? <> {hint}</> : null}
      </Hint>
    );
  }
  // Ergonomic props only. The block composes rather than suppresses, so
  // passing a title prop AND a <Title> child renders two headings.
  return <UiEmptyState title={title} description={hint} />;
}

/**
 * "You need to be signed in", in one place.
 *
 * `ErrorState` derives this from a 401 it was handed, which serves every page
 * that discovers the fact by making a request. It does NOT serve a page that
 * knows it up front: `RequireSession` (components/session.tsx) has already
 * narrowed the four-state session to "the server answered, and there is
 * nobody", and holds no error object to hand over. Both need the same three
 * strings, so they read them from here rather than each writing their own --
 * two spellings of the sign-in wall is how the absence glyph ended up with
 * four (see issue-detail/Absent.tsx).
 */
const SIGN_IN_TITLE = "Sign-in required";
const SIGN_IN_DESCRIPTION =
  "This needs a signed-in identity. Sign in through the platform and reload.";

export function SignInRequired() {
  return <UiErrorState intent="warning" title={SIGN_IN_TITLE} description={SIGN_IN_DESCRIPTION} />;
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
      title={authRequired ? SIGN_IN_TITLE : "Something went wrong"}
      description={
        authRequired
          ? SIGN_IN_DESCRIPTION
          : `${errorCode(error) ?? "ERROR"}: ${errorMessage(error)}`
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
/**
 * The shape `AsyncSection` needs from a query, declared STRUCTURALLY.
 *
 * A TanStack `UseQueryResult` satisfies this without being named, so this
 * file -- and the tests around it -- stay free of the library while every
 * caller passes a real query. Naming the fields we use also documents the
 * contract: anything else on the result is deliberately not part of it.
 */
export type QueryLike<T> = {
  data: T | undefined;
  error: unknown;
  isPending: boolean;
  isError: boolean;
  /** True during a REFETCH as well as a first load, which is the distinction
   *  the old `refreshing` flag existed to make. */
  isFetching: boolean;
  refetch?: () => unknown;
};

export function AsyncSection<T>({
  query,
  onRetry,
  loadingLabel,
  isEmpty,
  emptyTitle,
  emptyHint,
  emptyTone,
  renderLoading,
  children,
}: {
  // The query itself, not a re-spelling of its state. This took a bespoke
  // `AsyncState` union until the app moved to one cache; the union existed
  // only because there was no query object to hand around.
  query: QueryLike<T>;
  /** Defaults to the query's own `refetch`, so the retry button on an error
   *  works without every caller remembering to wire one. */
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
  // `isPending` is FIRST-LOAD only. A refetch with data already cached stays
  // out of this branch, which is what keeps a filter change from blanking the
  // table -- the behaviour the old union spelled as `refreshing`.
  if (query.isPending) {
    return renderLoading ? <>{renderLoading()}</> : <Loading label={loadingLabel} />;
  }
  if (query.isError) {
    return <ErrorState error={query.error} onRetry={onRetry ?? (() => void query.refetch?.())} />;
  }
  // Pending is false and error is false, so data is present; the cast is the
  // one place that fact is asserted rather than repeated at every call site.
  const data = query.data as T;
  if (isEmpty?.(data)) {
    return (
      <EmptyState title={emptyTitle ?? "Nothing here yet"} hint={emptyHint} tone={emptyTone} />
    );
  }
  // The fetching flag reaches the child rather than being swallowed here. A
  // section that knows it is reloading can mark itself busy in place; the
  // alternative is unmounting it, which is what this component used to do.
  return <>{children(data, query.isFetching)}</>;
}
