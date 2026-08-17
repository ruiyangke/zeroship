import type { ReactNode } from "react";

/**
 * The fieldset that owns the rail's shared label, value and action tracks.
 *
 * It renders the fieldset itself because every row must remain a direct grid
 * child for the subgrid chain to work. `grouped` is the Links variant that
 * follows a RailSection; it does not change the column geometry.
 *
 * `grid-cols-rail` is a theme value, not a utility spelled twice: both
 * variants below must resolve to the SAME tracks, and two arbitrary values
 * sitting next to each other are two things to keep in step. See
 * --grid-template-columns-rail in theme/tailwind.css.
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
      className={
        grouped
          ? "rail-fields -mt-3 mx-0 mb-0 grid grid-cols-rail items-center gap-x-2 border-0 p-0 disabled:opacity-50"
          : "rail-fields m-0 grid grid-cols-rail items-center gap-x-2 border-0 p-0 disabled:opacity-50"
      }
      disabled={disabled}
    >
      {children}
    </fieldset>
  );
}
