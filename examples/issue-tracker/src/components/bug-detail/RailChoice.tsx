import { useState, type ReactNode } from "react";
import { Button, Select } from "@zeroship/ui";

/**
 * A rail property you read, and edit only when you mean to.
 *
 * The rail used to render a bordered Select per property, so a column whose
 * job is to state facts about the bug was a column of form controls -- four
 * boxes with chevrons, heavier on the page than the conversation they sat
 * beside. Linear shows the value and swaps in a control on interaction; this
 * is that, using the same edit-on-demand pattern the whiteboard and URL rows
 * already use, so the rail has one behaviour rather than two.
 *
 * NOT done by restyling the design system's select chrome away. A control
 * that looks like text but eats clicks is worse than an honest box; the fix
 * is to not render a control until one is wanted.
 */
export function RailChoice({
  label,
  value,
  display,
  options,
  disabled = false,
  onChange,
}: {
  label: string;
  /** The stored value, used to preselect the control. */
  value: string;
  /** How the value reads when it is not being edited -- text or a badge. */
  display: ReactNode;
  options: { value: string; label: string }[];
  disabled?: boolean;
  onChange: (next: string) => void | Promise<void>;
}) {
  const [editing, setEditing] = useState(false);

  if (!editing) {
    return (
      <div className="rail-choice">
        <span className="field-label">{label}</span>
        <span className="rail-choice-value">{display}</span>
        <Button
          variant="plain"
          size="small"
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
    <div className="rail-choice is-editing">
      <span className="field-label">{label}</span>
      <Select
        value={value}
        aria-label={label}
        disabled={disabled}
        onValueChange={async (next) => {
          if (next != null && next !== value) await onChange(next);
          setEditing(false);
        }}
        renderValue={(current) =>
          options.find((option) => option.value === current)?.label ?? String(current ?? "")
        }
      >
        {options.map((option) => (
          <Select.Item key={option.value} value={option.value}>
            {option.label}
          </Select.Item>
        ))}
      </Select>
      <Button
        variant="plain"
        size="small"
        aria-label={`Cancel editing ${label}`}
        onClick={() => setEditing(false)}
      >
        Cancel
      </Button>
    </div>
  );
}
