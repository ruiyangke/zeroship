/*
 * Banner — an inline, page-level message.
 *
 * The horizontal sibling of ErrorState: a leading intent icon, a text
 * column (title + description), an optional action row, and an optional
 * dismiss button — laid out in a row on an opaque tinted background. Use
 * it for "your trial ends in 3 days", "changes saved", "we couldn't
 * reach the server" — the kind of message that sits in the flow of a
 * page rather than taking over the viewport.
 *
 * Dual surface like ErrorState / EmptyState / Card:
 *
 *   1. Ergonomic props:
 *        <Banner intent="success" title="Saved" description="All set." />
 *
 *   2. Compound parts:
 *        <Banner intent="warning">
 *          <Banner.Title>Trial ending</Banner.Title>
 *          <Banner.Description>3 days left.</Banner.Description>
 *          <Banner.Actions><Button>Upgrade</Button></Banner.Actions>
 *        </Banner>
 *
 * Use ONE mode. The surfaces compose rather than suppress: ergonomic
 * `title`/`description` render FIRST in the text column, then any
 * compound children render after them. Supplying both a `title` prop AND
 * a `<Banner.Title>` child renders TWO titles — pick one mode per banner.
 *
 * Aria — the `role` is OPT-IN via `live`, mirroring ErrorState:
 *   - `live` + info/success → `role="status"` (polite live region)
 *   - `live` + warning/danger → `role="alert"` (assertive live region)
 *   - default (no `live`) → a plain region with NO role.
 * A statically rendered banner (a trial-status bar that's always present)
 * must NOT assert a live region — it would make the SR interrupt the user
 * to announce a message that was always there. Set `live` only when the
 * banner appears DYNAMICALLY in response to something (a save completed,
 * a connection dropped) so the announcement is warranted.
 *
 * The leading icon is decorative (aria-hidden) — the title/description
 * carry the meaning. Dismiss is a real `<button aria-label="Dismiss">`
 * with an aria-hidden × glyph, rendered only when `onDismiss` is wired.
 *
 * This block has no root `asChild` — its value is the composed body
 * (icon + text column + actions + dismiss); routing the root through a
 * Slot would render only the consumer's child and discard that body.
 * Wrap the block in your own element if you need a custom/semantic root.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";
import { Stack } from "../../layouts/Stack";
import type { Intent } from "../_intent";

/**
 * Banner derives from the shared {@link Intent} vocabulary but excludes
 * `"neutral"` — a neutral banner carries no status, which is meaningless
 * for a page-level message that exists to flag something.
 */
export type BannerIntent = Exclude<Intent, "neutral">;

export interface BannerProps
  extends Omit<ComponentPropsWithoutRef<"div">, "title"> {
  /**
   * Semantic color family — tints the background, the leading icon, and
   * (when `live`) selects the live-region role.
   * - `info` (default): the accent palette — neutral information.
   * - `success`: system-green — a positive / completed outcome.
   * - `warning`: system-orange — a caution the user should note.
   * - `danger`: system-red — an error / blocked condition.
   */
  intent?: BannerIntent;

  /** Render a dismiss button (a real `<button aria-label="Dismiss">`).
   *  Requires `onDismiss` — a dismiss button with no handler would be a
   *  no-op focusable control, so the button is NOT rendered when
   *  `onDismiss` is missing (a dev-mode warning fires). */
  dismissible?: boolean;

  /** Called when the dismiss button is activated. Banner is
   *  uncontrolled-visibility-agnostic — it does not hide itself; the
   *  consumer removes it from the tree in response. */
  onDismiss?: () => void;

  /**
   * Assert this banner to assistive tech as a live region. Default
   * `false`. When `true`, the role is chosen by intent: `status`
   * (polite) for info/success, `alert` (assertive) for warning/danger.
   * Set this ONLY when the banner appears dynamically — a statically
   * rendered banner must stay a plain region so the SR isn't interrupted
   * to announce a message that was always present.
   */
  live?: boolean;

  /**
   * Headline. Rendered first in the text column. This is the ergonomic
   * mode. To control composition, use the compound `<Banner.Title>` part
   * instead. Use ONE mode: there is no suppression, so passing both this
   * prop AND a `<Banner.Title>` child renders two titles.
   */
  title?: ReactNode;

  /** Supporting copy beneath the title. */
  description?: ReactNode;

  /** Banner contents — compound parts and/or arbitrary children. */
  children?: ReactNode;
}

/* ─── Banner root ────────────────────────────────────────────────────── */

const BannerRoot = forwardRef<HTMLDivElement, BannerProps>(function BannerRoot(
  {
    intent = "info",
    dismissible = false,
    onDismiss,
    live = false,
    title,
    description,
    className,
    children,
    ...rest
  },
  ref,
) {
  // A dismiss button with no `onDismiss` would be a no-op focusable
  // control (a keyboard tab-stop that does nothing). Render it only when
  // a handler is wired; warn in dev so the consumer sees the gap.
  const showDismiss = dismissible && typeof onDismiss === "function";

  if (process.env.NODE_ENV !== "production") {
    if (dismissible && typeof onDismiss !== "function") {
      // eslint-disable-next-line no-console
      console.warn(
        "Banner dismissible requires onDismiss; the dismiss button is not rendered.",
      );
    }
  }

  const composedClassName = classnames(
    "zs-banner",
    `zs-banner--${intent}`,
    className,
  );

  // Live-region role ONLY when `live` (see the file-header note). The
  // severity picks polite vs assertive: info/success announce politely
  // via `status`; warning/danger interrupt via `alert`.
  const liveProps = live
    ? ({ role: intent === "warning" || intent === "danger" ? "alert" : "status" } as const)
    : undefined;

  const dataProps = {
    "data-slot": "banner",
    "data-intent": intent,
  };

  // The interior: a horizontal row of [icon][text column][dismiss]. A
  // row Stack (not Cluster — a banner row does not wrap) lays the three
  // out inline; the text column is a vertical Stack so title/description
  // and any compound parts flow top-to-bottom. We compose the Wave 1
  // primitives rather than re-rolling flexbox.
  const body = (
    <Stack
      className="zs-banner__row"
      data-slot="banner-row"
      direction="row"
      align="start"
      gap={3}
    >
      <BannerIcon intent={intent} />
      <Stack className="zs-banner__text" data-slot="banner-text" gap={1}>
        {title != null ? <BannerTitle>{title}</BannerTitle> : null}
        {description != null ? (
          <BannerDescription>{description}</BannerDescription>
        ) : null}
        {children}
      </Stack>
      {showDismiss ? (
        <button
          type="button"
          aria-label="Dismiss"
          data-slot="banner-dismiss"
          className="zs-banner__dismiss"
          onClick={onDismiss}
        >
          <span aria-hidden="true">×</span>
        </button>
      ) : null}
    </Stack>
  );

  return (
    <div
      {...rest}
      {...liveProps}
      {...dataProps}
      ref={ref}
      className={composedClassName}
    >
      {body}
    </div>
  );
});
BannerRoot.displayName = "Banner";

/* ─── Subparts ───────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;
type ParagraphProps = ComponentPropsWithoutRef<"p">;

export type BannerTitleProps = DivProps;
export type BannerDescriptionProps = ParagraphProps;
export type BannerActionsProps = DivProps;

/* The icon is rendered internally with a built-in glyph keyed off
 * `intent`. It is not part of the public compound surface — the icon's
 * meaning is fixed per intent, so there's no consumer slot for it. The
 * wrapper is aria-hidden; the title/description carry the meaning. */
function BannerIcon({ intent }: { intent: BannerIntent }) {
  return (
    <div
      aria-hidden="true"
      data-slot="banner-icon"
      className="zs-banner__icon"
    >
      <svg viewBox="0 0 24 24" focusable="false">
        {intent === "success" ? (
          // A circle + check — the universal success glyph.
          <>
            <circle cx="12" cy="12" r="10" />
            <path d="M8 12.5l2.5 2.5L16 9" />
          </>
        ) : (
          // info / warning / danger share the circle + exclamation alert
          // glyph; the tint + the copy carry the severity distinction.
          <>
            <circle cx="12" cy="12" r="10" />
            <line x1="12" y1="7" x2="12" y2="13" />
            <circle cx="12" cy="17" r="1" />
          </>
        )}
      </svg>
    </div>
  );
}

const BannerTitle = forwardRef<HTMLDivElement, BannerTitleProps>(
  function BannerTitle({ className, ...rest }, ref) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="banner-title"
        className={classnames("zs-banner__title", className)}
      />
    );
  },
);
BannerTitle.displayName = "Banner.Title";

const BannerDescription = forwardRef<
  HTMLParagraphElement,
  BannerDescriptionProps
>(function BannerDescription({ className, ...rest }, ref) {
  return (
    <p
      {...rest}
      ref={ref}
      data-slot="banner-description"
      className={classnames("zs-banner__description", className)}
    />
  );
});
BannerDescription.displayName = "Banner.Description";

const BannerActions = forwardRef<HTMLDivElement, BannerActionsProps>(
  function BannerActions({ className, ...rest }, ref) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="banner-actions"
        className={classnames("zs-banner__actions", className)}
      />
    );
  },
);
BannerActions.displayName = "Banner.Actions";

/* ─── public Banner namespace ────────────────────────────────────────── */

type BannerComponent = typeof BannerRoot & {
  Title: typeof BannerTitle;
  Description: typeof BannerDescription;
  Actions: typeof BannerActions;
};

export const Banner = BannerRoot as BannerComponent;
Banner.Title = BannerTitle;
Banner.Description = BannerDescription;
Banner.Actions = BannerActions;
