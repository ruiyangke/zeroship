// The host-side network allowlist, in ONE place for every driver.
//
// It lived twice, once per driver, with the same three lines copied. That is how
// it broke: the check read the URL's WHATWG authority, while `pg` re-parses the
// connection string with its own parser and honours a `host` QUERY PARAMETER that
// overrides that authority. Two parsers disagreed about which host a URL names,
// and the one that decided was not the one that checked:
//
//   postgres://user:pw@approved.example:5432/db?host=somewhere.else
//   ^ the authority the allowlist read      ^ the host `pg` actually dialled
//
// So the fix cannot be "read the host better". A URL may DESIGNATE more than one
// host, and any of them may be the one dialled, so every host it designates has to
// be approved. `hostsDesignatedBy` enumerates them and the caller requires all to
// pass -- which stays correct even if a driver starts honouring a parameter it
// ignores today, because an unapproved host then fails closed rather than
// silently becoming reachable.
//
// MySQL does not honour `?host=` at the time of writing. It is checked the same
// way regardless: the cost is one array entry, and the alternative is trusting a
// third-party parser's current behaviour to stay put.

/**
 * Every host the URL could direct a driver to: the authority, plus any `host`
 * query parameter (PostgreSQL connection URIs accept one, and `pg` prefers it
 * over the authority).
 *
 * Returns the raw string when the URL cannot be parsed, so an unparseable URL is
 * checked against the allowlist as-is and refused rather than skipped.
 */
export function hostsDesignatedBy(url: string): string[] {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return [url];
  }
  const hosts = [parsed.hostname];
  for (const [key, value] of parsed.searchParams) {
    // Compared case-insensitively: query keys are not normalized by the URL
    // parser, and `?HOST=` would otherwise slip past while a driver matching
    // case-insensitively still honoured it.
    if (key.toLowerCase() === "host" && value.length > 0) hosts.push(value);
  }
  return hosts;
}

/**
 * Throw unless EVERY host the URL designates is allowlisted. `label` names the
 * driver in the refusal so an operator can tell which connection was stopped.
 *
 * A host is compared exactly. Hostnames from the authority arrive lowercased by
 * the URL parser, so an allowlist entry must be lowercase to match one; that is
 * left strict on purpose, because a loose comparison here is the kind of thing
 * that turns `evil-example.com` into a match for `example.com`.
 */
export function assertHostAllowed(
  url: string,
  allowlist: readonly string[] | undefined,
  label: string,
): void {
  if (!allowlist || allowlist.length === 0) return;
  for (const host of hostsDesignatedBy(url)) {
    if (!allowlist.includes(host)) {
      throw new Error(
        `host ${label} driver: ${host} is not in the host allowlist ${JSON.stringify(allowlist)}`,
      );
    }
  }
}
