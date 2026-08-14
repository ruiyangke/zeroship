// Reconstructs derived "current state" views from the issues.get activity
// log, for the two relations the RPC surface never exposes as a direct
// per-issue list: attached keywords and set flags. Every mutation that
// touches either one writes an activity row (see src/index.ts
// recordRelatedChange calls for "keywords" and "flag.<TypeName>"), so
// replaying the log in order reconstructs the live set.
//
// This is EXACT for keywords: each attach/detach names its own keyword, and
// duplicate attach/detach calls are no-ops server-side (no extra activity
// row), so there is no double-counting.
//
// It is only a best-effort approximation for MULTIPLICABLE flag types: the
// activity fieldName is `flag.<TypeName>`, not keyed by the individual flag
// row, so several simultaneous flags of the same multiplicable type collapse
// onto one derived value (the most recent one). Non-multiplicable flag
// types (the common case) are exact, because the server itself only ever
// keeps one flag row per (issue, flagType) for those.
import type { Activity } from "./types";

export function deriveNamedSet(
  activities: readonly Activity[],
  fieldName: string,
): string[] {
  const set = new Set<string>();
  for (const activity of [...activities].sort((a, b) => a.changedAt - b.changedAt)) {
    if (activity.fieldName !== fieldName) continue;
    if (activity.oldValue) set.delete(activity.oldValue);
    if (activity.newValue) set.add(activity.newValue);
  }
  return [...set].sort();
}

export function deriveLastValue(
  activities: readonly Activity[],
  fieldName: string,
): string | null {
  let latest: Activity | null = null;
  for (const activity of activities) {
    if (activity.fieldName !== fieldName) continue;
    if (!latest || activity.changedAt >= latest.changedAt) latest = activity;
  }
  return latest ? latest.newValue : null;
}
