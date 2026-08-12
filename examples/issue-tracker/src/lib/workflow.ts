export const BUG_STATUSES = [
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
  "RESOLVED",
  "VERIFIED",
  "CLOSED",
] as const;

export const BUG_RESOLUTIONS = [
  "FIXED",
  "INVALID",
  "WONTFIX",
  "DUPLICATE",
  "WORKSFORME",
  "INCOMPLETE",
] as const;

export type BugStatus = (typeof BUG_STATUSES)[number];
export type BugResolution = (typeof BUG_RESOLUTIONS)[number];

export type BugState = {
  status: BugStatus;
  resolution: BugResolution | null;
};

export const STATUS_TRANSITIONS: Readonly<
  Record<BugStatus, readonly BugStatus[]>
> = {
  UNCONFIRMED: ["CONFIRMED", "IN_PROGRESS", "RESOLVED"],
  CONFIRMED: ["IN_PROGRESS", "RESOLVED"],
  IN_PROGRESS: ["CONFIRMED", "RESOLVED"],
  RESOLVED: ["VERIFIED", "CLOSED", "CONFIRMED"],
  VERIFIED: ["CLOSED", "CONFIRMED"],
  CLOSED: ["CONFIRMED"],
};

const STATUS_SET = new Set<string>(BUG_STATUSES);
const RESOLUTION_SET = new Set<string>(BUG_RESOLUTIONS);
const OPEN_STATUS_SET = new Set<BugStatus>([
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
]);

export class InvalidBugTransitionError extends Error {
  readonly code = "INVALID_BUG_TRANSITION";
  readonly status = 409;

  constructor(message: string) {
    super(message);
    this.name = "InvalidBugTransitionError";
  }
}

export function isBugStatus(value: unknown): value is BugStatus {
  return typeof value === "string" && STATUS_SET.has(value);
}

export function isBugResolution(value: unknown): value is BugResolution {
  return typeof value === "string" && RESOLUTION_SET.has(value);
}

export function isOpenBugStatus(status: BugStatus): boolean {
  return OPEN_STATUS_SET.has(status);
}

function assertValidState(state: BugState): void {
  if (!isBugStatus(state.status)) {
    throw new InvalidBugTransitionError(`unknown bug status: ${String(state.status)}`);
  }

  if (state.resolution !== null && !isBugResolution(state.resolution)) {
    throw new InvalidBugTransitionError(
      `unknown bug resolution: ${String(state.resolution)}`,
    );
  }

  if (isOpenBugStatus(state.status) && state.resolution !== null) {
    throw new InvalidBugTransitionError(
      `open status ${state.status} cannot have a resolution`,
    );
  }

  if (!isOpenBugStatus(state.status) && state.resolution === null) {
    throw new InvalidBugTransitionError(
      `closed status ${state.status} requires a resolution`,
    );
  }
}

/**
 * Apply one edge from the issue tracker's fixed Bugzilla-style workflow.
 *
 * Entering RESOLVED requires a resolution. VERIFIED and CLOSED carry the
 * existing resolution forward. Reopening into an open state clears it.
 */
export function transitionBugState(
  current: BugState,
  next: { status: BugStatus; resolution?: BugResolution | null },
): BugState {
  assertValidState(current);

  if (!isBugStatus(next.status)) {
    throw new InvalidBugTransitionError(`unknown bug status: ${String(next.status)}`);
  }

  if (!STATUS_TRANSITIONS[current.status].includes(next.status)) {
    throw new InvalidBugTransitionError(
      `cannot transition bug from ${current.status} to ${next.status}`,
    );
  }

  if (next.status === "RESOLVED") {
    if (!isBugResolution(next.resolution)) {
      throw new InvalidBugTransitionError(
        "transitioning to RESOLVED requires a valid resolution",
      );
    }
    if (next.resolution === "DUPLICATE") {
      throw new InvalidBugTransitionError(
        "DUPLICATE resolution must be set through bugs.markDuplicate",
      );
    }
    return { status: "RESOLVED", resolution: next.resolution };
  }

  if (isOpenBugStatus(next.status)) {
    if (next.resolution !== undefined && next.resolution !== null) {
      throw new InvalidBugTransitionError(
        `open status ${next.status} cannot have a resolution`,
      );
    }
    return { status: next.status, resolution: null };
  }

  if (next.resolution !== undefined && next.resolution !== current.resolution) {
    throw new InvalidBugTransitionError(
      `transitioning to ${next.status} cannot change the resolution`,
    );
  }

  return { status: next.status, resolution: current.resolution };
}

/** Resolve an open bug. DUPLICATE is reserved for bugs.markDuplicate. */
export function resolveBugState(
  current: BugState,
  resolution: Exclude<BugResolution, "DUPLICATE">,
): BugState {
  if (resolution === ("DUPLICATE" as BugResolution)) {
    throw new InvalidBugTransitionError(
      "DUPLICATE resolution must be set through bugs.markDuplicate",
    );
  }
  return transitionBugState(current, { status: "RESOLVED", resolution });
}

/** Reopen a resolved, verified, or closed bug as CONFIRMED with no resolution. */
export function reopenBugState(current: BugState): BugState {
  if (isOpenBugStatus(current.status)) {
    throw new InvalidBugTransitionError(
      `cannot reopen bug in open status ${current.status}`,
    );
  }
  return transitionBugState(current, { status: "CONFIRMED" });
}

/** State portion of bugs.markDuplicate; the duplicate target is updated with it. */
export function markDuplicateBugState(current: BugState): BugState {
  assertValidState(current);
  if (!STATUS_TRANSITIONS[current.status].includes("RESOLVED")) {
    throw new InvalidBugTransitionError(
      `cannot transition bug from ${current.status} to RESOLVED`,
    );
  }
  return { status: "RESOLVED", resolution: "DUPLICATE" };
}
