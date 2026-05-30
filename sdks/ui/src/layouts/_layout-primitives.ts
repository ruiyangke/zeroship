/**
 * Shared layout vocabulary. Layout primitives map these closed unions to
 * `--zs-space-*` / flex / grid values. Keeping them closed is the governance
 * lever: consumers cannot pass arbitrary px/rem spacing into a layout.
 */
export type Gap = 0 | "half" | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10;
export type Pad = Gap;
export type Align = "start" | "center" | "end" | "stretch";
export type Justify =
  | "start"
  | "center"
  | "end"
  | "between"
  | "around"
  | "evenly";
/**
 * Which inline edge a fixed-size region sits on. Shared by the layouts
 * that pin one part to an edge (`Split.side`, `AppShell.sidebarSide`).
 * Logical (`start`/`end`), so it follows the writing direction under RTL.
 */
export type Side = "start" | "end";

/** Map a Gap/Pad token to its `--zs-space-*` custom property reference. */
export function spaceVar(token: Gap): string {
  return `var(--zs-space-${token === "half" ? "half" : token})`;
}

const ALIGN: Record<Align, string> = {
  start: "flex-start",
  center: "center",
  end: "flex-end",
  stretch: "stretch",
};

const JUSTIFY: Record<Justify, string> = {
  start: "flex-start",
  center: "center",
  end: "flex-end",
  between: "space-between",
  around: "space-around",
  evenly: "space-evenly",
};

export const alignValue = (a: Align): string => ALIGN[a];
export const justifyValue = (j: Justify): string => JUSTIFY[j];
