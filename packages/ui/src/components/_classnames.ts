/*
 * Canonical class-name helpers — used everywhere a component composes
 * a static base class with consumer-supplied / state-derived strings.
 *
 * `classnames` was extracted in slice-3 review-fix item 25 to retire the
 * private copies every component file used to ship.
 *
 * `composeBaseClass` was hoisted in Phase 2.C (AlertDialog review-fix
 * item 10) for the same reason: Dialog, AlertDialog, and Field each
 * carried byte-identical copies of the same Base-UI-shape compose
 * helper. One source of truth keeps drift out of the package surface.
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

/**
 * Compose our static class with a Base UI `className` that may be
 * either a string or a state-callback. Callbacks become wrapping
 * callbacks so our class always wins; strings concat. Same shape
 * Dialog / AlertDialog / Field all relied on.
 */
export function composeBaseClass<S>(
  ours: string,
  theirs: string | ((state: S) => string | undefined) | undefined,
): string | ((state: S) => string | undefined) {
  if (theirs == null) return ours;
  if (typeof theirs === "string") return classnames(ours, theirs);
  return (state: S) => classnames(ours, theirs(state));
}
