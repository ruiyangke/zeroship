import { useState, type ReactNode } from "react";
import { Button } from "@zeroship/ui";

/**
 * The shape of one rail property: a fact you read, and edit when you mean to.
 *
 * This exists because the rail had two of them. Severity and priority read as
 * "label, value, Edit"; assignee read as "label, value, Change" and unfolded a
 * picker that stayed open after use. Same job, two spellings and two words for
 * the same verb -- and a reader scanning the column has to notice the
 * difference before deciding it does not matter.
 *
 * The shell is shared and the editor is not, because that is where the two
 * genuinely differ: one picks from a fixed list, the other searches people.
 * Sharing the Select as well would have forced assignee into the wrong control,
 * which is why the split is here rather than in RailChoice.
 */
export function RailProperty({
  label,
  display,
  disabled = false,
  wide = false,
  children,
}: {
  label: string;
  /** How the value reads when it is not being edited -- text or a badge. */
  display: ReactNode;
  disabled?: boolean;
  /**
   * Give the editor the whole rail width instead of the value column.
   * A search field with a button does not fit beside a label in a column
   * this narrow -- it ran off the page edge.
   */
  wide?: boolean;
  /** The editor, shown only while editing. Call `done` when it is finished. */
  children: (done: () => void) => ReactNode;
}) {
  const [editing, setEditing] = useState(false);

  if (!editing) {
    return (
      <div className="rail-choice">
        <span className="field-label">{label}</span>
        <span className="rail-choice-value">{display}</span>
        <Button
          variant="plain"
          size="sm"
          disabled={disabled}
          // Named per property: a rail of bare "Edit" buttons tells a screen
          // reader nothing about which one it has landed on.
          aria-label={`Edit ${label}`}
          onClick={() => setEditing(true)}
        >
          Edit
        </Button>
      </div>
    );
  }

  return (
    <div className={wide ? "rail-choice is-editing is-wide" : "rail-choice is-editing"}>
      <span className="field-label">{label}</span>
      {children(() => setEditing(false))}
      <Button
        variant="plain"
        size="sm"
        aria-label={`Cancel editing ${label}`}
        onClick={() => setEditing(false)}
      >
        Cancel
      </Button>
    </div>
  );
}
