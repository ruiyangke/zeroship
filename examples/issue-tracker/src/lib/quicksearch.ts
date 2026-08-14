import {
  ISSUE_RESOLUTIONS,
  ISSUE_STATUSES,
  type IssueResolution,
  type IssueStatus,
} from "./workflow";

export const ISSUE_PRIORITIES = ["P1", "P2", "P3", "P4", "P5"] as const;

// Severity is IMPACT and nothing else. `enhancement` used to live here, which
// made "a critical feature request" unsayable -- one field cannot answer both
// "what is this" and "how bad is it". It moved to ISSUE_KINDS below.
export const ISSUE_SEVERITIES = [
  "blocker",
  "critical",
  "major",
  "normal",
  "minor",
  "trivial",
] as const;

// WHAT the row is: something broken, something wished for, or something to do.
// `defect` is the default because an unclassified report is more often
// something broken than something wished for.
export const ISSUE_KINDS = ["defect", "enhancement", "task"] as const;

export type IssuePriority = (typeof ISSUE_PRIORITIES)[number];
export type IssueSeverity = (typeof ISSUE_SEVERITIES)[number];
export type IssueKind = (typeof ISSUE_KINDS)[number];

export type QuickSearchClause =
  | { field: "text"; value: string }
  | { field: "priority"; value: IssuePriority }
  | { field: "assignee"; value: string }
  | { field: "component"; value: string }
  | { field: "product"; value: string }
  | { field: "status"; value: IssueStatus }
  | { field: "resolution"; value: IssueResolution }
  | { field: "severity"; value: IssueSeverity }
  | { field: "kind"; value: IssueKind }
  | { field: "reporter"; value: string };

const PRIORITY_SET = new Set<string>(ISSUE_PRIORITIES);
const STATUS_SET = new Set<string>(ISSUE_STATUSES);
const RESOLUTION_SET = new Set<string>(ISSUE_RESOLUTIONS);
const SEVERITY_SET = new Set<string>(ISSUE_SEVERITIES);
const KIND_SET = new Set<string>(ISSUE_KINDS);

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
    // Beside `severity:`, because the two are now separate axes: `sev:critical
    // kind:enhancement` is a sentence this grammar has to be able to say.
    // Qualified only -- a bare `task` token has to stay a text search.
    "kind",
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
      return { field: "priority", value: normalized as IssuePriority };
    }
    case "status": {
      const normalized = value.toUpperCase();
      if (!STATUS_SET.has(normalized)) invalidValue("status", value);
      return { field: "status", value: normalized as IssueStatus };
    }
    case "resolution":
    case "res": {
      const normalized = value.toUpperCase();
      if (!RESOLUTION_SET.has(normalized)) invalidValue("resolution", value);
      return { field: "resolution", value: normalized as IssueResolution };
    }
    case "severity":
    case "sev": {
      const normalized = value.toLowerCase();
      if (!SEVERITY_SET.has(normalized)) invalidValue("severity", value);
      return { field: "severity", value: normalized as IssueSeverity };
    }
    case "kind": {
      const normalized = value.toLowerCase();
      if (!KIND_SET.has(normalized)) invalidValue("kind", value);
      return { field: "kind", value: normalized as IssueKind };
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
        value: normalizedPriority as IssuePriority,
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
