/*
 * StatCard — a KPI / metric card built ON Card.
 *
 * The canonical dashboard tile: a small metric label, a large value, and
 * an optional change indicator ("delta"). StatCard does NOT re-roll a
 * surface — it COMPOSES the `Card` block (and inherits Card's opaque-base
 * glass invariant, padding tokens, variant/size system). StatCard only
 * adds the metric typography and the delta row.
 *
 *   <StatCard
 *     label="Monthly revenue"
 *     value="$48,120"
 *     delta={{ value: "12%", direction: "up" }}
 *     icon={<TrendIcon />}
 *   />
 *
 * Renders as:
 *
 *   <Card variant="elevated">           ← the composed surface
 *     <div class="zs-stat-card__head">  ← label row (+ optional icon)
 *       <div class="zs-stat-card__label">Monthly revenue</div>
 *       <div class="zs-stat-card__icon" aria-hidden>…</div>
 *     </div>
 *     <div class="zs-stat-card__value">$48,120</div>
 *     <div class="zs-stat-card__delta" data-direction="up">
 *       <span class="zs-stat-card__delta-glyph" aria-hidden>▲</span>
 *       <span class="zs-visually-hidden">increased </span>
 *       12%
 *     </div>
 *   </Card>
 *
 * Delta a11y (WCAG 1.4.1 Use of Color): the direction MUST be conveyed by
 * more than color. We do TWO things beyond the tint:
 *   1. a directional glyph (▲ / ▼ / —) — a SHAPE difference, aria-hidden
 *      so AT doesn't try to read the arrow character; and
 *   2. a visually-hidden direction word ("increased" / "decreased" /
 *      "no change") prepended to the delta text, so a screen reader
 *      announces e.g. "increased 12%" rather than a bare "12%". Sighted
 *      users get the glyph + tint; AT users get the word.
 * `flat` is neutral (no up/down) and uses an em-dash glyph + "no change".
 *
 * The value is a styled `<div>`, NOT a heading — a metric value is data,
 * not a section title, so making it an <hN> would pollute the document
 * outline. A consumer who genuinely wants it in the heading hierarchy can
 * wrap the StatCard's value via the compound surface of the parent page.
 *
 * The icon is decorative (aria-hidden) — the label carries the meaning.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";
import { Card, type CardVariant, type CardSize } from "../../components/Card";

export type StatCardDeltaDirection = "up" | "down" | "flat";

export interface StatCardDelta {
  /** The change magnitude as display text, e.g. `"12%"` or `"+340"`. */
  value: string;
  /** Direction of the change — drives the glyph, the tint, and the
   *  visually-hidden direction word. */
  direction: StatCardDeltaDirection;
}

export interface StatCardProps extends ComponentPropsWithoutRef<"div"> {
  /** The metric name (small, secondary). e.g. `"Monthly revenue"`. */
  label: ReactNode;

  /** The metric value, rendered large (title scale). e.g. `"$48,120"`.
   *  Rendered as a styled `<div>`, not a heading — a value is data, not a
   *  section title. */
  value: ReactNode;

  /** Optional change indicator. `direction` is conveyed by a glyph + a
   *  visually-hidden direction word in ADDITION to the color tint, so the
   *  meaning never relies on color alone (WCAG 1.4.1). */
  delta?: StatCardDelta;

  /** Optional decorative leading glyph, wrapped in an `aria-hidden`
   *  container — the label carries the accessible meaning. */
  icon?: ReactNode;

  /** Underlying Card visual style. Default `elevated` (a metric tile
   *  reads as a raised cell on a dashboard). */
  variant?: CardVariant;

  /** Underlying Card sizing. Default `md`. */
  size?: CardSize;
}

/** Per-direction glyph + screen-reader direction word. The glyph is a
 *  SHAPE signal (aria-hidden); the word is the AT-announced signal. */
const DELTA_META: Record<
  StatCardDeltaDirection,
  { glyph: string; label: string }
> = {
  up: { glyph: "▲", label: "increased" }, // ▲
  down: { glyph: "▼", label: "decreased" }, // ▼
  flat: { glyph: "—", label: "no change" }, // —
};

export const StatCard = forwardRef<HTMLDivElement, StatCardProps>(
  function StatCard(
    {
      label,
      value,
      delta,
      icon,
      variant = "elevated",
      size = "md",
      className,
      ...rest
    },
    ref,
  ) {
    const meta = delta ? DELTA_META[delta.direction] : null;

    return (
      <Card
        {...rest}
        ref={ref}
        variant={variant}
        size={size}
        data-slot="stat-card"
        className={classnames("zs-stat-card", className)}
      >
        <div className="zs-stat-card__head" data-slot="stat-card-head">
          <div className="zs-stat-card__label" data-slot="stat-card-label">
            {label}
          </div>
          {icon != null ? (
            <div
              aria-hidden="true"
              className="zs-stat-card__icon"
              data-slot="stat-card-icon"
            >
              {icon}
            </div>
          ) : null}
        </div>

        <div className="zs-stat-card__value" data-slot="stat-card-value">
          {value}
        </div>

        {delta && meta ? (
          <div
            className="zs-stat-card__delta"
            data-slot="stat-card-delta"
            data-direction={delta.direction}
          >
            <span aria-hidden="true" className="zs-stat-card__delta-glyph">
              {meta.glyph}
            </span>
            {/* SR-announced direction word — the non-color signal that
                makes "increased 12%" legible to assistive tech. The
                trailing space keeps the announcement from running into
                the value text. */}
            <span className="zs-visually-hidden">{meta.label} </span>
            {delta.value}
          </div>
        ) : null}
      </Card>
    );
  },
);
StatCard.displayName = "StatCard";
