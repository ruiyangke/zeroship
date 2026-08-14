export type TrackedFieldValue = string | number | boolean | null | undefined;

export type TrackedFieldChange = {
  fieldName: string;
  oldValue: string | null;
  newValue: string | null;
};

export function activityValue(value: TrackedFieldValue): string | null {
  return value === null || value === undefined ? null : String(value);
}

/**
 * Build activity payloads in caller-specified field order, with one row per
 * changed field. `before` and `after` should describe the same logical row.
 */
export function diffTrackedFields(
  before: Readonly<Record<string, TrackedFieldValue>>,
  after: Readonly<Record<string, TrackedFieldValue>>,
  fieldNames: readonly string[],
): TrackedFieldChange[] {
  const changes: TrackedFieldChange[] = [];
  const seen = new Set<string>();

  for (const fieldName of fieldNames) {
    if (seen.has(fieldName)) continue;
    seen.add(fieldName);

    const oldValue = activityValue(before[fieldName]);
    const newValue = activityValue(after[fieldName]);
    if (oldValue === newValue) continue;
    changes.push({ fieldName, oldValue, newValue });
  }

  return changes;
}

/**
 * The fields filing an issue writes, in one insert, from nothing.
 *
 * Shared because two sides need the SAME answer: the server writes exactly
 * these as the creation event, and the timeline drops exactly these so the
 * story does not open with nineteen changes nobody made. When the two sides
 * guessed separately -- the timeline used "no previous value, and near the
 * description" -- an attachment uploaded moments after filing matched the
 * guess and vanished from the page. The server already knew the answer.
 */
export const ISSUE_CREATION_FIELDS = [
  "productId",
  "componentId",
  "versionId",
  "milestoneId",
  "summary",
  "kind",
  "status",
  "severity",
  "priority",
  "reporterId",
  "assigneeId",
  "qaContactId",
  "whiteboard",
  "opSys",
  "platform",
  "url",
  "isConfirmed",
  "voteCount",
  "commentCount",
  "deadline",
] as const;
