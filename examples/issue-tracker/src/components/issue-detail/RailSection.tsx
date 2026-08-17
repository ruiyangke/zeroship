import type { ReactNode } from "react";

/**
 * A labelled divider between groups of rail fields, and the home of any
 * action that belongs to the group rather than to a row in it.
 *
 * The rail was one uninterrupted column of thirteen controls, so status,
 * classification, people and location all had the same standing and the eye
 * had nowhere to rest. These are the joints.
 *
 * `action` exists because two controls here act on a whole group: "Move
 * issue..." changes product AND component, and the one that reveals the four
 * "Other" fields reveals all four. Both used to sit in the rows below their
 * heading -- one alone in the row action track with no row beside it, one as a
 * full-width sentence indented past the heading above it -- and each read as a
 * row whose label had gone missing. A group action sits ON the group's line.
 *
 * This is also what frees the value column: the third track is `auto`, so the
 * widest thing in it sized it for every row, and that was the 92px "Move
 * issue..." button.
 */
export function RailSection({ title, action }: { title: string; action?: ReactNode }) {
  return (
    <p className="rail-section col-span-full mt-4 mb-2 flex items-center justify-between gap-2 border-t border-line pt-4 text-xs font-semibold tracking-wide text-ink-muted uppercase">
      <span>{title}</span>
      {action}
    </p>
  );
}
