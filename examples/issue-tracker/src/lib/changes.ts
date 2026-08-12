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
