export interface FieldError {
  message: string;
  path: string;
}

export class ValidationError extends Error {
  name = "ValidationError";
  errors: Record<string, FieldError>;

  constructor(errors: Record<string, FieldError>) {
    const messages = Object.values(errors)
      .map((e) => e.message)
      .join(", ");
    super(`Validation failed: ${messages}`);
    this.errors = errors;
  }
}

export function mapNativeError(msg: string): Error {
  const lower = msg.toLowerCase();
  if (lower.includes("unique") || lower.includes("duplicate")) {
    const err = new Error(msg) as Error & { code: number };
    err.code = 11000;
    return err;
  }
  return new Error(msg);
}
