import type { ReactNode } from "react";

/**
 * The fieldset that owns the rail's shared label, value and action tracks.
 *
 * It renders the fieldset itself because every row must remain a direct grid
 * child for the subgrid chain to work. `grouped` is the Links variant that
 * follows a RailSection; it does not change the column geometry.
 */
export function RailFieldset({
  children,
  disabled = false,
  grouped = false,
}: {
  children: ReactNode;
  disabled?: boolean;
  grouped?: boolean;
}) {
  return (
    <fieldset
      className={grouped ? "rail-fields rail-groups m-0 border-0 p-0" : "rail-fields m-0 border-0 p-0"}
      disabled={disabled}
    >
      {children}
    </fieldset>
  );
}
