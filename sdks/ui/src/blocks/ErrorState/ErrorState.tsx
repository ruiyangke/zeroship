/*
 * ErrorState — the "something went wrong" panel.
 *
 * Visually a sibling of EmptyState (same centered column of icon /
 * heading / description / actions) but tuned for failure: the icon is
 * tinted by `intent` (red for errors, orange for warnings) and an
 * optional `onRetry` renders a real <Button> wired to retry.
 *
 * Dual surface like Card / EmptyState:
 *
 *   1. Ergonomic props:
 *        <ErrorState
 *          title="Couldn't load your projects"
 *          description="Check your connection and try again."
 *          onRetry={refetch}
 *        />
 *
 *   2. Compound parts:
 *        <ErrorState intent="warning">
 *          <ErrorState.Title>Storage almost full</ErrorState.Title>
 *          <ErrorState.Description>…</ErrorState.Description>
 *          <ErrorState.Actions><Button>Manage</Button></ErrorState.Actions>
 *        </ErrorState>
 *
 * Use ONE mode. The surfaces compose rather than suppress: ergonomic-prop
 * content (`title`/`description` + the `onRetry` button) renders FIRST,
 * then any compound children render after it in the same centered column.
 * Supplying both a `title` prop AND an `<ErrorState.Title>` child renders
 * TWO headings. Pick one mode per panel.
 *
 * Aria — `role="alert"` is OPT-IN via `live`. A statically rendered
 * error page (a route that renders because the data failed) must NOT
 * assert `role="alert"`: alert is an assertive live region and would
 * make the SR interrupt the user to announce a panel that was always
 * there. Set `live` only when the error appears DYNAMICALLY in response
 * to a user action (a failed save, a dropped connection) so the
 * announcement is warranted. The default is a plain region.
 *
 * The title renders as a heading (`<h2>` by default; relevel via
 * `<ErrorState.Title asChild>`). The icon wrapper is `aria-hidden`.
 *
 * This block has no root `asChild` — its value is the composed column
 * (icon + title + description + retry/actions); routing the root through
 * a Slot would render only the consumer's child and discard that column.
 * Wrap the block in your own element if you need a custom/semantic root.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Center } from "../../layouts/Center";
import { Stack } from "../../layouts/Stack";
import { Button } from "../../components/Button";

export type ErrorStateIntent = "error" | "warning";

export interface ErrorStateProps
  extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  /**
   * Severity — tints the icon and (under forced-colors) the system
   * color mapping.
   * - `error` (default): system-red — a failure the user must act on.
   * - `warning`: system-orange — a degraded-but-recoverable condition.
   */
  intent?: ErrorStateIntent;

  /**
   * Headline. Rendered as the panel's heading (`<h2>` by default).
   * This is the ergonomic mode. To relevel the heading or otherwise
   * control composition, use the compound `<ErrorState.Title>` part
   * instead. Use ONE mode: there is no suppression, so passing both this
   * prop AND an `<ErrorState.Title>` child renders two headings (prop
   * content first, then children).
   */
  title?: ReactNode;

  /** Muted supporting copy beneath the title. */
  description?: ReactNode;

  /**
   * When provided, render a `Retry` `<Button>` wired to this handler.
   * Compose your own action row via `<ErrorState.Actions>` if you need
   * different / additional buttons.
   */
  onRetry?: () => void;

  /**
   * Assert this error to assistive tech as an alert live region
   * (`role="alert"`). Default `false`. Set this ONLY when the error
   * appears dynamically (a failed action, a lost connection) — a
   * statically-rendered error route must stay a plain region so the SR
   * isn't interrupted to announce a panel that was always present.
   */
  live?: boolean;

  /** Panel contents — compound parts and/or arbitrary children. */
  children?: ReactNode;
}

/* ─── ErrorState root ────────────────────────────────────────────────── */

const ErrorStateRoot = forwardRef<HTMLDivElement, ErrorStateProps>(
  function ErrorStateRoot(
    {
      intent = "error",
      title,
      description,
      onRetry,
      live = false,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    const composedClassName = classnames(
      "zs-error-state",
      `zs-error-state--${intent}`,
      className,
    );

    // `role="alert"` ONLY when `live` (see the file-header note). A
    // plain region otherwise.
    const liveProps = live ? ({ role: "alert" } as const) : undefined;
    const dataProps = {
      "data-slot": "error-state",
      "data-intent": intent,
    };

    const column = (
      <Center asChild>
        <Stack className="zs-error-state__column" align="center" gap={3}>
          <ErrorStateIcon intent={intent} />
          {title != null ? <ErrorStateTitle>{title}</ErrorStateTitle> : null}
          {description != null ? (
            <ErrorStateDescription>{description}</ErrorStateDescription>
          ) : null}
          {onRetry != null ? (
            <ErrorStateActions>
              <Button variant="filled" onClick={onRetry}>
                Retry
              </Button>
            </ErrorStateActions>
          ) : null}
          {children}
        </Stack>
      </Center>
    );

    return (
      <div
        {...rest}
        {...liveProps}
        {...dataProps}
        ref={ref}
        className={composedClassName}
      >
        {column}
      </div>
    );
  },
);
ErrorStateRoot.displayName = "ErrorState";

/* ─── Subparts ───────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;
type ParagraphProps = ComponentPropsWithoutRef<"p">;
type HeadingProps = ComponentPropsWithoutRef<"h2">;

export type ErrorStateDescriptionProps = ParagraphProps;
export type ErrorStateActionsProps = DivProps;

/* The icon is rendered internally with a built-in alert glyph keyed off
 * `intent`. It is not part of the public compound surface — the icon's
 * meaning is fixed (an alert mark), so there's no consumer slot for it.
 * The wrapper is aria-hidden; the heading carries the meaning. */
function ErrorStateIcon({ intent }: { intent: ErrorStateIntent }) {
  return (
    <div
      aria-hidden="true"
      data-slot="error-state-icon"
      className="zs-error-state__icon"
    >
      <svg viewBox="0 0 24 24" focusable="false">
        {/* A circle + exclamation: the universal alert glyph. SVG
            attribute units (viewBox space), not CSS px. */}
        <circle cx="12" cy="12" r="10" />
        <line x1="12" y1="7" x2="12" y2="13" />
        <circle cx="12" cy="17" r="1" />
      </svg>
    </div>
  );
}

export interface ErrorStateTitleProps extends HeadingProps {
  /** Render-as the single child element — e.g. to relevel the heading. */
  asChild?: boolean;
}

const ErrorStateTitle = forwardRef<HTMLHeadingElement, ErrorStateTitleProps>(
  function ErrorStateTitle({ asChild = false, className, children, ...rest }, ref) {
    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.error(
            "ErrorState.Title asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          data-slot="error-state-title"
          className={classnames("zs-error-state__title", className)}
        >
          {children}
        </Slot>
      );
    }
    return (
      <h2
        {...rest}
        ref={ref}
        data-slot="error-state-title"
        className={classnames("zs-error-state__title", className)}
      >
        {children}
      </h2>
    );
  },
);
ErrorStateTitle.displayName = "ErrorState.Title";

const ErrorStateDescription = forwardRef<
  HTMLParagraphElement,
  ErrorStateDescriptionProps
>(function ErrorStateDescription({ className, ...rest }, ref) {
  return (
    <p
      {...rest}
      ref={ref}
      data-slot="error-state-description"
      className={classnames("zs-error-state__description", className)}
    />
  );
});
ErrorStateDescription.displayName = "ErrorState.Description";

const ErrorStateActions = forwardRef<HTMLDivElement, ErrorStateActionsProps>(
  function ErrorStateActions({ className, ...rest }, ref) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="error-state-actions"
        className={classnames("zs-error-state__actions", className)}
      />
    );
  },
);
ErrorStateActions.displayName = "ErrorState.Actions";

/* ─── public ErrorState namespace ────────────────────────────────────── */

type ErrorStateComponent = typeof ErrorStateRoot & {
  Title: typeof ErrorStateTitle;
  Description: typeof ErrorStateDescription;
  Actions: typeof ErrorStateActions;
};

export const ErrorState = ErrorStateRoot as ErrorStateComponent;
ErrorState.Title = ErrorStateTitle;
ErrorState.Description = ErrorStateDescription;
ErrorState.Actions = ErrorStateActions;
