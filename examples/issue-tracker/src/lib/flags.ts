export type FlagTargetType = "bug" | "attachment";

export type FlagTargetInput = {
  bugId?: string | null;
  attachmentId?: string | null;
};

export type FlagTarget =
  | { bugId: string; attachmentId: null }
  | { bugId: null; attachmentId: string };

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
  if (targetType !== "bug" && targetType !== "attachment") {
    throw new InvalidFlagTargetError(
      `unsupported flag target type: ${String(targetType)}`,
    );
  }

  const bugId = optionalId(input.bugId, "bugId");
  const attachmentId = optionalId(input.attachmentId, "attachmentId");

  if ((bugId === null) === (attachmentId === null)) {
    throw new InvalidFlagTargetError(
      "exactly one of bugId and attachmentId must be set",
    );
  }

  if (targetType === "bug") {
    if (bugId === null) {
      throw new InvalidFlagTargetError("bug flag type requires bugId");
    }
    return { bugId, attachmentId: null };
  }

  if (attachmentId === null) {
    throw new InvalidFlagTargetError(
      "attachment flag type requires attachmentId",
    );
  }
  return { bugId: null, attachmentId };
}
