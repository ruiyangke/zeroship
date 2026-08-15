/*
 * Internal helper for merging independent className sources supplied by
 * consumers and Base UI render props. It never adds a library class.
 *
 * Filename starts with an underscore so the directory listing makes it
 * clear this is internal plumbing. The package barrel does not export it.
 */
export function classnames(
  ...parts: Array<string | false | null | undefined>
): string {
  return parts.filter(Boolean).join(" ");
}
