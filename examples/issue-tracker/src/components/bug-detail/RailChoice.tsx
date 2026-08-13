import type { ReactNode } from "react";
import { Select } from "@zeroship/ui";

import { RailProperty } from "./RailProperty";

/**
 * A rail property whose value comes from a fixed list.
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
 *
 * The read-mode shell lives in RailProperty, shared with the assignee row,
 * which needs a people search rather than a list.
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
  return (
    <RailProperty label={label} display={display} disabled={disabled}>
      {(done) => (
        <Select
          value={value}
          aria-label={label}
          disabled={disabled}
          onValueChange={async (next) => {
            if (next != null && next !== value) await onChange(next);
            done();
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
      )}
    </RailProperty>
  );
}
