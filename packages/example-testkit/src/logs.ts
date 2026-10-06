import { existsSync, readFileSync, statSync } from "node:fs";

/** Where a service log ends now, so a later read takes only what came after. */
export function logOffset(log: string): number {
  return existsSync(log) ? statSync(log).size : 0;
}

/**
 * The distinct ERROR records a service log holds after `offset`, one line
 * each, in the order they first appear. The services log JSON records, and the
 * dev server prefixes the runtime's with its own tag, so a record is read from
 * its first brace. A dispatch error carries the creator's own error under
 * `error.*`; its name, message and first stack frame are what make a failure
 * actionable, because the wire flattens a handler throw to "internal error".
 */
export function serverErrors(log: string, offset = 0): string[] {
  if (!existsSync(log)) return [];
  const errors = new Set<string>();
  for (const line of readFileSync(log).subarray(offset).toString("utf8").split("\n")) {
    const start = line.indexOf("{");
    if (start < 0) continue;
    let record: { level?: unknown; target?: unknown; fields?: Record<string, unknown> };
    try { record = JSON.parse(line.slice(start)); } catch { continue; }
    if (record.level !== "ERROR") continue;
    const fields = record.fields ?? {};
    const name = typeof fields["error.name"] === "string" ? fields["error.name"] : undefined;
    const message = String(fields["error.message"] ?? fields.message ?? "(no message)");
    const frame = /\bat (?:[^\\(]*\()?([^\\()]+:\d+:\d+)/.exec(String(fields["error.stack"] ?? ""))?.[1];
    errors.add(`${name ? `${name}: ` : ""}${message}${frame ? ` (at ${frame})` : ""} [${String(record.target)}]`);
  }
  return [...errors];
}
