export type FlagTargetType = "issue" | "attachment";

export type FlagTargetInput = {
  issueId?: string | null;
  attachmentId?: string | null;
};

export type FlagTarget =
  | { issueId: string; attachmentId: null }
  | { issueId: null; attachmentId: string };

export class InvalidFlagTargetError extends Error {
  readonly code = "INVALID_FLAG_TARGET";
  readonly status = 400;

  constructor(message: string) {
    super(message);
    this.name = "InvalidFlagTargetError";
  }
}

function optionalId(value: unknown, field: string): string | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new InvalidFlagTargetError(`${field} must be a non-empty id`);
  }
  return value;
}

/** Validate and normalize the flags table's application-enforced XOR target. */
export function assertFlagTarget(
  targetType: FlagTargetType,
  input: FlagTargetInput,
): FlagTarget {
  if (targetType !== "issue" && targetType !== "attachment") {
    throw new InvalidFlagTargetError(
      `unsupported flag target type: ${String(targetType)}`,
    );
  }

  const issueId = optionalId(input.issueId, "issueId");
  const attachmentId = optionalId(input.attachmentId, "attachmentId");

  if ((issueId === null) === (attachmentId === null)) {
    throw new InvalidFlagTargetError(
      "exactly one of issueId and attachmentId must be set",
    );
  }

  if (targetType === "issue") {
    if (issueId === null) {
      throw new InvalidFlagTargetError("issue flag type requires issueId");
    }
    return { issueId, attachmentId: null };
  }

  if (attachmentId === null) {
    throw new InvalidFlagTargetError(
      "attachment flag type requires attachmentId",
    );
  }
  return { issueId: null, attachmentId };
}
