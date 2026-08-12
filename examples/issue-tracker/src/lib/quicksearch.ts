import {
  BUG_RESOLUTIONS,
  BUG_STATUSES,
  type BugResolution,
  type BugStatus,
} from "./workflow";

export const BUG_PRIORITIES = ["P1", "P2", "P3", "P4", "P5"] as const;
export const BUG_SEVERITIES = [
  "blocker",
  "critical",
  "major",
  "normal",
  "minor",
  "trivial",
  "enhancement",
] as const;

export type BugPriority = (typeof BUG_PRIORITIES)[number];
export type BugSeverity = (typeof BUG_SEVERITIES)[number];

export type QuickSearchClause =
  | { field: "text"; value: string }
  | { field: "priority"; value: BugPriority }
  | { field: "assignee"; value: string }
  | { field: "component"; value: string }
  | { field: "product"; value: string }
  | { field: "status"; value: BugStatus }
  | { field: "resolution"; value: BugResolution }
  | { field: "severity"; value: BugSeverity }
  | { field: "reporter"; value: string };

const PRIORITY_SET = new Set<string>(BUG_PRIORITIES);
const STATUS_SET = new Set<string>(BUG_STATUSES);
const RESOLUTION_SET = new Set<string>(BUG_RESOLUTIONS);
const SEVERITY_SET = new Set<string>(BUG_SEVERITIES);

export class QuickSearchSyntaxError extends Error {
  readonly code = "INVALID_QUICKSEARCH";
  readonly status = 400;

  constructor(message: string) {
    super(message);
    this.name = "QuickSearchSyntaxError";
  }
}

function tokenize(input: string): string[] {
  const tokens: string[] = [];
  let token = "";
  let tokenStarted = false;
  let quoted = false;
  let escaped = false;

  const pushToken = () => {
    if (!tokenStarted) return;
    if (token.length === 0) {
      throw new QuickSearchSyntaxError("QuickSearch terms cannot be empty");
    }
    tokens.push(token);
    token = "";
    tokenStarted = false;
  };

  for (const character of input) {
    if (escaped) {
      token += character;
      tokenStarted = true;
      escaped = false;
      continue;
    }

    if (character === "\\") {
      escaped = true;
      tokenStarted = true;
      continue;
    }

    if (character === '"') {
      quoted = !quoted;
      tokenStarted = true;
      continue;
    }

    if (/\s/u.test(character) && !quoted) {
      pushToken();
      continue;
    }

    token += character;
    tokenStarted = true;
  }

  if (escaped) {
    throw new QuickSearchSyntaxError("QuickSearch cannot end with an escape");
  }
  if (quoted) {
    throw new QuickSearchSyntaxError("unterminated quote in QuickSearch");
  }
  pushToken();
  return tokens;
}

function invalidValue(field: string, value: string): never {
  throw new QuickSearchSyntaxError(`invalid ${field} value: ${value}`);
}

function normalizeHandle(field: "assignee" | "reporter", value: string): string {
  const handle = value.replace(/^@/u, "");
  if (handle.length === 0) invalidValue(field, value);
  return handle;
}

function parseQualifiedToken(token: string): QuickSearchClause | null {
  const colon = token.indexOf(":");
  if (colon < 0) return null;

  const prefix = token.slice(0, colon).toLowerCase();
  const value = token.slice(colon + 1);
  const knownPrefixes = new Set([
    "assignee",
    "component",
    "comp",
    "product",
    "prod",
    "priority",
    "prio",
    "status",
    "resolution",
    "res",
    "severity",
    "sev",
    "reporter",
  ]);

  if (!knownPrefixes.has(prefix)) return null;
  if (value.length === 0) {
    throw new QuickSearchSyntaxError(`${prefix}: requires a value`);
  }

  switch (prefix) {
    case "assignee":
      return { field: "assignee", value: normalizeHandle("assignee", value) };
    case "component":
    case "comp":
      return { field: "component", value };
    case "product":
    case "prod":
      return { field: "product", value };
    case "priority":
    case "prio": {
      const normalized = value.toUpperCase();
      if (!PRIORITY_SET.has(normalized)) invalidValue("priority", value);
      return { field: "priority", value: normalized as BugPriority };
    }
    case "status": {
      const normalized = value.toUpperCase();
      if (!STATUS_SET.has(normalized)) invalidValue("status", value);
      return { field: "status", value: normalized as BugStatus };
    }
    case "resolution":
    case "res": {
      const normalized = value.toUpperCase();
      if (!RESOLUTION_SET.has(normalized)) invalidValue("resolution", value);
      return { field: "resolution", value: normalized as BugResolution };
    }
    case "severity":
    case "sev": {
      const normalized = value.toLowerCase();
      if (!SEVERITY_SET.has(normalized)) invalidValue("severity", value);
      return { field: "severity", value: normalized as BugSeverity };
    }
    case "reporter":
      return { field: "reporter", value: normalizeHandle("reporter", value) };
    default:
      return null;
  }
}

/** Parse the compact search vocabulary promised by this example's SPEC. */
export function parseQuickSearch(input: string): QuickSearchClause[] {
  return tokenize(input).map((token): QuickSearchClause => {
    const normalizedPriority = token.toUpperCase();
    if (PRIORITY_SET.has(normalizedPriority)) {
      return {
        field: "priority",
        value: normalizedPriority as BugPriority,
      };
    }

    if (token.startsWith("@")) {
      if (token.length === 1) {
        throw new QuickSearchSyntaxError("@ requires an assignee handle");
      }
      return { field: "assignee", value: token.slice(1) };
    }

    return parseQualifiedToken(token) ?? { field: "text", value: token };
  });
}
