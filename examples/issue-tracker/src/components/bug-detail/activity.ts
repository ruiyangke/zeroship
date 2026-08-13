import { personName, type PeopleMap } from "./people";

/**
 * How a field change is worded, shared by the History tab and the inline
 * timeline.
 *
 * Extracted when activity started appearing in two places: two copies of this
 * map is how "reporterId" ends up reading as "Reporter" in one view and
 * "Reporter Id" in the other, which is exactly the bug it was written to fix.
 */
const FIELD_LABELS: Record<string, string> = {
  bug_group: "Group restriction",
  assigneeId: "Assignee",
  qaContactId: "QA contact",
  reporterId: "Reporter",
  productId: "Product",
  componentId: "Component",
  versionId: "Version",
  milestoneId: "Milestone",
  duplicateOfId: "Duplicate of",
  opSys: "OS",
  whiteboard: "Whiteboard",
  isConfirmed: "Confirmed",
  voteCount: "Votes",
  commentCount: "Comments",
  workTimeMinutes: "Work time",
};

export function fieldLabel(field: string): string {
  if (FIELD_LABELS[field]) return FIELD_LABELS[field];
  // camelCase and snake_case both become words, so "workTimeMinutes" reads as
  // "Work time minutes" rather than as a property name.
  const spaced = field.replace(/_/g, " ").replace(/([a-z])([A-Z])/g, "$1 $2");
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** Who acted, or a neutral stand-in when the id resolves to nobody. */
export function personLabel(actorId: string, people: PeopleMap): string {
  return personName(actorId, people, "Someone");
}
