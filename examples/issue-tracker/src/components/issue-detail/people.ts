/**
 * Naming the people an issue refers to.
 *
 * The detail page used to print `issue.assigneeId`, `issue.reporterId` and
 * `comment.authorId` verbatim -- `user_0345pl8prFezDsK4tQtOTB` where a name
 * belongs -- while the CC panel a few pixels away showed "Alice Dev", because
 * `cc.list` joined its user and no other endpoint did. `issues.get` and
 * `comments.list` now resolve theirs too, and this is the one place that
 * decides what to render.
 */

export type Person = { id: string; handle: string | null; name: string | null };

export type PeopleMap = Record<string, Person | undefined>;

/**
 * Falls back to the id rather than to a placeholder. A row whose user has been
 * deleted still refers to somebody, and "unknown" would erase the only handle
 * left to search or grep by.
 */
export function personName(
  id: string | null | undefined,
  people: PeopleMap,
  absent = "--",
): string {
  if (!id) return absent;
  const person = people[id];
  return person?.name || person?.handle || id;
}
