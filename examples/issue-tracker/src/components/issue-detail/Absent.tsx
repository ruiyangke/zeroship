import { Skeleton } from "@zeroship/ui";

/**
 * One token for "there is no value here", and a DIFFERENT one for "the value
 * has not arrived yet".
 *
 * The rail said both with the same glyph. `CcPanel` and `RelationsPanel`
 * rendered a dim `--` while their query was in flight, and four rows above
 * them `QA contact` rendered a dim `--` because nobody is assigned. Same
 * column, same styling, two unrelated facts -- so an issue whose CC list was
 * still loading was indistinguishable from one with an empty CC list, and the
 * only way to tell was to wait and see whether it changed.
 *
 * On top of that the absent case had three spellings in one column: `--` for
 * people and free-text fields, `None` for version and milestone, `nobody` /
 * `none` for the two set summaries. They all mean the same thing. A column you
 * are meant to GLANCE at should not ask you to learn four words for empty, so
 * absence is now `--` in every shape -- scalar, person, or set -- matching what
 * the issue tables already print.
 */
export const ABSENT = "--";

/** No value is set. */
export function Absent() {
  return <span className="dim text-ink-muted">{ABSENT}</span>;
}

/**
 * The value is still loading.
 *
 * A Skeleton rather than a word: it reserves the line without asserting
 * anything about the content, and it is `aria-hidden`, so a screen reader is
 * not told "dash" for a field that is about to have a name in it.
 */
export function Pending({ width = "6rem" }: { width?: string }) {
  return <Skeleton width={width} />;
}
