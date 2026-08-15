import { useState, type ReactNode } from "react";
import { Button } from "@zeroship/ui";

/**
 * A rail group that states a fact and opens its full panel on request.
 *
 * These panels lived in the rail once before. They were moved to the main
 * column because measurement put the stack at 1856px inside a 352px column --
 * every panel rendered its list AND its add-form at all times, so nine of them
 * became a ribbon nobody could scan. Moving them back unchanged would rebuild
 * exactly that.
 *
 * What makes the rail viable this time is that only one can be tall at once.
 * Collapsed, a group is a single line: "CC  nobody  [Add]". The list and the
 * form render only while open, so the rail's resting height is a row per
 * group rather than the sum of every panel's contents.
 *
 * The read-first behaviour is the same as RailProperty; the difference is that
 * these editors are whole panels with their own data and their own errors, so
 * they own their open state rather than being handed a `done` callback.
 */
export function RailDisclosure({
  label,
  summary,
  action = "Edit",
  children,
}: {
  label: string;
  /** The one-line resting state -- a count, a name, or "none". */
  summary: ReactNode;
  /** The verb on the affordance. "Add" reads better on an empty CC list. */
  action?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);

  return (
    <div className={open ? "rail-disclosure is-open" : "rail-disclosure"}>
      <div className="rail-choice">
        <span className="field-label">{label}</span>
        <span className="rail-choice-value">{summary}</span>
        <Button
          variant="plain"
          size="sm"
          // Named per group, so a screen reader landing on the button knows
          // which panel it opens rather than hearing "Edit" four times.
          aria-label={open ? `Close ${label}` : `${action} ${label}`}
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
        >
          {open ? "Close" : action}
        </Button>
      </div>
      {open ? <div className="rail-disclosure-body">{children}</div> : null}
    </div>
  );
}
