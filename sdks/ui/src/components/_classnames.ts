/*
 * Canonical `classnames` helper — used everywhere a component composes
 * a static base class with consumer-supplied / state-derived strings.
 *
 * Extracted in slice-3 review-fix item 25: every component-level file
 * (Button, Field, Input, Card, Dialog, AlertDialog) had its own private
 * copy with identical semantics. One source of truth keeps drift out
 * of the package surface.
 *
 * Filename starts with an underscore so the directory listing makes it
 * obvious this isn't a public component — it's internal plumbing. The
 * package barrel does NOT re-export from here.
 */
export function classnames(
  ...parts: Array<string | false | null | undefined>
): string {
  return parts.filter(Boolean).join(" ");
}
