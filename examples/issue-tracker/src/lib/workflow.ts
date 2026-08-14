export const ISSUE_STATUSES = [
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
  "RESOLVED",
  "VERIFIED",
  "CLOSED",
] as const;

export const ISSUE_RESOLUTIONS = [
  "FIXED",
  "INVALID",
  "WONTFIX",
  "DUPLICATE",
  "WORKSFORME",
  "INCOMPLETE",
] as const;

export type IssueStatus = (typeof ISSUE_STATUSES)[number];
export type IssueResolution = (typeof ISSUE_RESOLUTIONS)[number];

export type IssueState = {
  status: IssueStatus;
  resolution: IssueResolution | null;
};

export const STATUS_TRANSITIONS: Readonly<
  Record<IssueStatus, readonly IssueStatus[]>
> = {
  UNCONFIRMED: ["CONFIRMED", "IN_PROGRESS", "RESOLVED"],
  CONFIRMED: ["IN_PROGRESS", "RESOLVED"],
  IN_PROGRESS: ["CONFIRMED", "RESOLVED"],
  RESOLVED: ["VERIFIED", "CLOSED", "CONFIRMED"],
  VERIFIED: ["CLOSED", "CONFIRMED"],
  CLOSED: ["CONFIRMED"],
};

const STATUS_SET = new Set<string>(ISSUE_STATUSES);
const RESOLUTION_SET = new Set<string>(ISSUE_RESOLUTIONS);
const OPEN_STATUS_SET = new Set<IssueStatus>([
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
]);

export class InvalidIssueTransitionError extends Error {
  readonly code = "INVALID_ISSUE_TRANSITION";
  readonly status = 409;

  constructor(message: string) {
    super(message);
    this.name = "InvalidIssueTransitionError";
  }
}

export function isIssueStatus(value: unknown): value is IssueStatus {
  return typeof value === "string" && STATUS_SET.has(value);
}

export function isIssueResolution(value: unknown): value is IssueResolution {
  return typeof value === "string" && RESOLUTION_SET.has(value);
}

export function isOpenIssueStatus(status: IssueStatus): boolean {
  return OPEN_STATUS_SET.has(status);
}

function assertValidState(state: IssueState): void {
  if (!isIssueStatus(state.status)) {
    throw new InvalidIssueTransitionError(`unknown issue status: ${String(state.status)}`);
  }

  if (state.resolution !== null && !isIssueResolution(state.resolution)) {
    throw new InvalidIssueTransitionError(
      `unknown issue resolution: ${String(state.resolution)}`,
    );
  }

  if (isOpenIssueStatus(state.status) && state.resolution !== null) {
    throw new InvalidIssueTransitionError(
      `open status ${state.status} cannot have a resolution`,
    );
  }

  if (!isOpenIssueStatus(state.status) && state.resolution === null) {
    throw new InvalidIssueTransitionError(
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
export function transitionIssueState(
  current: IssueState,
  next: { status: IssueStatus; resolution?: IssueResolution | null },
): IssueState {
  assertValidState(current);

  if (!isIssueStatus(next.status)) {
    throw new InvalidIssueTransitionError(`unknown issue status: ${String(next.status)}`);
  }

  if (!STATUS_TRANSITIONS[current.status].includes(next.status)) {
    throw new InvalidIssueTransitionError(
      `cannot transition issue from ${current.status} to ${next.status}`,
    );
  }

  if (next.status === "RESOLVED") {
    if (!isIssueResolution(next.resolution)) {
      throw new InvalidIssueTransitionError(
        "transitioning to RESOLVED requires a valid resolution",
      );
    }
    if (next.resolution === "DUPLICATE") {
      throw new InvalidIssueTransitionError(
        "DUPLICATE resolution must be set through issues.markDuplicate",
      );
    }
    return { status: "RESOLVED", resolution: next.resolution };
  }

  if (isOpenIssueStatus(next.status)) {
    if (next.resolution !== undefined && next.resolution !== null) {
      throw new InvalidIssueTransitionError(
        `open status ${next.status} cannot have a resolution`,
      );
    }
    return { status: next.status, resolution: null };
  }

  if (next.resolution !== undefined && next.resolution !== current.resolution) {
    throw new InvalidIssueTransitionError(
      `transitioning to ${next.status} cannot change the resolution`,
    );
  }

  return { status: next.status, resolution: current.resolution };
}

/** Resolve an open issue. DUPLICATE is reserved for issues.markDuplicate. */
export function resolveIssueState(
  current: IssueState,
  resolution: Exclude<IssueResolution, "DUPLICATE">,
): IssueState {
  if (resolution === ("DUPLICATE" as IssueResolution)) {
    throw new InvalidIssueTransitionError(
      "DUPLICATE resolution must be set through issues.markDuplicate",
    );
  }
  return transitionIssueState(current, { status: "RESOLVED", resolution });
}

/** Reopen a resolved, verified, or closed issue as CONFIRMED with no resolution. */
export function reopenIssueState(current: IssueState): IssueState {
  if (isOpenIssueStatus(current.status)) {
    throw new InvalidIssueTransitionError(
      `cannot reopen issue in open status ${current.status}`,
    );
  }
  return transitionIssueState(current, { status: "CONFIRMED" });
}

/** State portion of issues.markDuplicate; the duplicate target is updated with it. */
export function markDuplicateIssueState(current: IssueState): IssueState {
  assertValidState(current);
  if (!STATUS_TRANSITIONS[current.status].includes("RESOLVED")) {
    throw new InvalidIssueTransitionError(
      `cannot transition issue from ${current.status} to RESOLVED`,
    );
  }
  return { status: "RESOLVED", resolution: "DUPLICATE" };
}
