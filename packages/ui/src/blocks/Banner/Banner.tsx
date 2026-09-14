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
 * Dismiss focus management — the consumer owns the unmount (the block
 * just fires `onDismiss`), so the Banner can't move focus AFTER it's
 * gone: by then the dismiss button is destroyed and focus has already
 * fallen back to `<body>`, dropping a keyboard/SR user out of context.
 * To avoid that, when the dismiss button is activated WHILE focused, the
 * Banner shifts focus to a sensible still-present target BEFORE invoking
 * `onDismiss`: the Banner root's nearest preceding focusable sibling, or
 * else the root's parent (momentarily made focusable via `tabindex=-1`).
 * If the consumer keeps the Banner mounted, this is a harmless no-op
 * (focus simply moves off the dismiss button). SSR-safe (no `document`
 * access during render).
 *
 * This block has no root `asChild` — its value is the composed body
 * (icon + text column + actions + dismiss); routing the root through a
 * Slot would render only the consumer's child and discard that body.
 * Wrap the block in your own element if you need a custom/semantic root.
 */
import {
  forwardRef,
  useCallback,
  useRef,
  type ComponentPropsWithoutRef,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import { CircleAlert, CircleCheck } from "lucide-react";
import { classnames } from "../../components/_classnames";
import { composeRefs } from "../../components/_slot";
import { Icon } from "../../components/Icon/Icon";
import { Stack } from "../../layouts/Stack";
import type { Intent } from "../../components/_intent";

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
   *  consumer removes it from the tree in response.
   *
   *  Focus contract: because dismissing typically unmounts the Banner
   *  (destroying the focused dismiss button), the Banner moves focus to a
   *  surviving target — its nearest preceding focusable sibling, else its
   *  parent (via a transient `tabindex=-1`) — BEFORE this handler runs, so
   *  a keyboard/SR user isn't dropped onto `<body>`. Only fires when the
   *  dismiss button held focus; pointer-driven dismiss with focus
   *  elsewhere is left untouched. */
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

/* ─── focus restoration ──────────────────────────────────────────────── */

/**
 * Whether `el` can hold keyboard focus right now. We treat a non-negative
 * `tabIndex` as the signal: it covers natively-focusable controls (which
 * report `0`) and authored `tabindex` targets, while excluding `-1`
 * (programmatic-only) and disabled controls (which report `-1`).
 */
function isFocusable(el: HTMLElement): boolean {
  return !el.hasAttribute("disabled") && el.tabIndex >= 0;
}

/**
 * Move focus off the (about-to-unmount) Banner onto a surviving target so
 * a keyboard/SR user is never dropped onto `<body>`. Preference order:
 *
 *   1. The Banner root's nearest PRECEDING focusable sibling — the most
 *      natural "back up one stop" landing spot.
 *   2. The root's parent element, made focusable with a transient
 *      `tabindex=-1` that we strip again on the next `blur`, so we don't
 *      leave a lingering programmatic tab-stop on the consumer's DOM.
 *
 * Called only from a browser event handler, so `document` is always
 * present here; the caller still guards `typeof document` for safety.
 */
function restoreFocusOnDismiss(root: HTMLElement): void {
  let sibling = root.previousElementSibling;
  while (sibling) {
    if (sibling instanceof HTMLElement && isFocusable(sibling)) {
      sibling.focus();
      return;
    }
    sibling = sibling.previousElementSibling;
  }

  const parent = root.parentElement;
  if (!parent) return;

  if (parent.tabIndex < 0 && !parent.hasAttribute("tabindex")) {
    parent.setAttribute("tabindex", "-1");
    // Strip the synthetic tab-stop once focus leaves, so we don't mutate
    // the consumer's DOM beyond the moment we needed it.
    const cleanup = () => {
      parent.removeAttribute("tabindex");
      parent.removeEventListener("blur", cleanup);
    };
    parent.addEventListener("blur", cleanup);
  }
  parent.focus();
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

  // Internal handle on the root so the dismiss path can relocate focus to
  // a surviving element BEFORE the consumer unmounts the Banner. Composed
  // with the forwarded ref so the consumer's ref still fans through.
  const rootRef = useRef<HTMLDivElement | null>(null);
  const composedRef = composeRefs<HTMLDivElement>(rootRef, ref);

  // Dismiss handler: move focus off the (about-to-be-destroyed) dismiss
  // button onto a still-present target, THEN fire onDismiss. Guarded so it
  // is a harmless no-op when the consumer keeps the Banner mounted.
  const handleDismiss = useCallback(
    (event: ReactMouseEvent<HTMLButtonElement>) => {
      const button = event.currentTarget;
      const root = rootRef.current;

      // Only relocate focus when the dismiss button actually holds it
      // (keyboard / SR activation). A pointer click with focus elsewhere
      // leaves the page's focus untouched.
      if (
        root &&
        typeof document !== "undefined" &&
        document.activeElement === button
      ) {
        restoreFocusOnDismiss(root);
      }

      onDismiss?.();
    },
    [onDismiss],
  );

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
          onClick={handleDismiss}
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
      ref={composedRef}
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
  // Cohesive intent→glyph mapping: success reads as a circled check, every
  // other severity (info / warning / danger) shares the circled-alert mark.
  // The tint + the copy carry the severity distinction, same as before; the
  // shape difference (check vs alert) reinforces success-vs-attention. Both
  // are Lucide stroked glyphs painting in `currentColor`, so the wrapper's
  // intent color flows through unchanged.
  const glyph = intent === "success" ? CircleCheck : CircleAlert;
  return (
    <div
      aria-hidden="true"
      data-slot="banner-icon"
      className="zs-banner__icon"
    >
      <Icon as={glyph} />
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
