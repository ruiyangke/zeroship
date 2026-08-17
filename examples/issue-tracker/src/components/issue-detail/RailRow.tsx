import type { ReactNode } from "react";

/**
 * One shared rail row: label, value and a quiet action on the rail's subgrid.
 *
 * RailProperty and RailDisclosure used to repeat this exact markup and rely
 * on a shared CSS class for its layout. Keeping the layout here makes adding a
 * third row kind impossible without inheriting the same tracks and behaviour.
 *
 * A resting row is `min-h-8` with NO vertical margin, so its pitch is the
 * theme's --it-row-h exactly -- the same 32px the description-list rows beside
 * it already used, and the same rhythm the issue table is built on. It carried
 * `my-1` before, which made the rail's own rows 40px while the rows between
 * them stayed 32, so one column had two rhythms and eight rows of the rail
 * cost a ninth row of height. Editing keeps the margin: that row grows into a
 * form and wants to separate from its neighbours.
 */
export function RailRow({
  label,
  value,
  action,
  editing = false,
  wide = false,
  revealAction = false,
}: {
  label: ReactNode;
  value: ReactNode;
  action: ReactNode;
  editing?: boolean;
  wide?: boolean;
  /**
   * Show the action at rest instead of on hover.
   *
   * For a row that HAS a value, hiding the action is what keeps the rail a
   * column of facts rather than a column of buttons. For a row with no value
   * it is the opposite: hidden, the whole row is one dim `--` and a reader is
   * told there is nothing there without being told they can put something
   * there. Three of the five Links rows are empty on a typical issue, which
   * is most of what anyone sees.
   */
  revealAction?: boolean;
}) {
  return (
    <div
      className={
        editing
          ? "rail-choice is-editing group/rail-row col-span-full my-1 grid min-h-8 grid-cols-subgrid items-center gap-y-1"
          : "rail-choice group/rail-row col-span-full grid min-h-8 grid-cols-subgrid items-center"
      }
    >
      <span className={wide ? "col-span-full text-base font-medium text-ink-muted" : "text-base font-medium text-ink-muted"}>
        {label}
      </span>
      <span className={wide ? "rail-choice-value col-span-full min-w-0" : "rail-choice-value min-w-0"}>
        {value}
      </span>
      <span
        className={
          editing
            ? wide
              ? "col-span-full justify-self-start"
              : "col-start-3 justify-self-start"
            : revealAction
              ? "justify-self-end"
              : "justify-self-end opacity-0 transition-opacity duration-fast group-hover/rail-row:opacity-100 group-focus-within/rail-row:opacity-100 [@media(hover:none)]:opacity-100"
        }
      >
        {action}
      </span>
    </div>
  );
}
