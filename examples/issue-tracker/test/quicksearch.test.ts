import { describe, expect, it } from "vitest";
import {
  QuickSearchSyntaxError,
  parseQuickSearch,
} from "../src/lib/quicksearch";

describe("QuickSearch parser", () => {
  it("parses the SPEC's Bugzilla shorthand example", () => {
    expect(parseQuickSearch("P1 @alice comp:parser")).toEqual([
      { field: "priority", value: "P1" },
      { field: "assignee", value: "alice" },
      { field: "component", value: "parser" },
    ]);
  });

  it("returns no clauses for blank or whitespace-only input", () => {
    expect(parseQuickSearch("")).toEqual([]);
    expect(parseQuickSearch(" \t\n ")).toEqual([]);
  });

  it("uses arbitrary bare terms as text and recognizes priority case-insensitively", () => {
    expect(parseQuickSearch("memory p2 leak P6")).toEqual([
      { field: "text", value: "memory" },
      { field: "priority", value: "P2" },
      { field: "text", value: "leak" },
      { field: "text", value: "P6" },
    ]);
  });

  it("supports quoted phrases and quoted qualifier values", () => {
    expect(parseQuickSearch('"memory leak" comp:"JS parser"')).toEqual([
      { field: "text", value: "memory leak" },
      { field: "component", value: "JS parser" },
    ]);
  });

  it("supports escaped quotes and backslashes inside quoted values", () => {
    expect(
      parseQuickSearch('"say \\"hello\\"" comp:"path\\\\parser"'),
    ).toEqual([
      { field: "text", value: 'say "hello"' },
      { field: "component", value: "path\\parser" },
    ]);
  });

  it("normalizes enum qualifiers and accepts common qualifier aliases", () => {
    expect(
      parseQuickSearch(
        "prio:p3 status:in_progress res:worksforme sev:CRITICAL prod:Core reporter:@bob assignee:@alice",
      ),
    ).toEqual([
      { field: "priority", value: "P3" },
      { field: "status", value: "IN_PROGRESS" },
      { field: "resolution", value: "WORKSFORME" },
      { field: "severity", value: "critical" },
      { field: "product", value: "Core" },
      { field: "reporter", value: "bob" },
      { field: "assignee", value: "alice" },
    ]);
  });

  it("reads kind as its own axis, beside severity", () => {
    expect(parseQuickSearch("kind:Enhancement sev:critical")).toEqual([
      { field: "kind", value: "enhancement" },
      { field: "severity", value: "critical" },
    ]);
  });

  it("no longer accepts enhancement as a severity", () => {
    // The value moved from the severity vocabulary to the kind vocabulary, so
    // the old spelling has to stop parsing rather than quietly keep matching
    // nothing: `sev:enhancement` used to be how a feature request was found.
    expect(() => parseQuickSearch("sev:enhancement")).toThrow(
      QuickSearchSyntaxError,
    );
  });

  it("preserves repeated clauses instead of overwriting them", () => {
    expect(parseQuickSearch("P1 P2 comp:parser comp:lexer")).toEqual([
      { field: "priority", value: "P1" },
      { field: "priority", value: "P2" },
      { field: "component", value: "parser" },
      { field: "component", value: "lexer" },
    ]);
  });

  it("preserves unknown qualifiers and URLs as text", () => {
    expect(parseQuickSearch("custom:value https://example.test/bug")).toEqual([
      { field: "text", value: "custom:value" },
      { field: "text", value: "https://example.test/bug" },
    ]);
  });

  it.each([
    "priority:P0",
    "status:open",
    "resolution:later",
    "severity:urgent",
  ])("rejects invalid enum qualifier %s", (input) => {
    expect(() => parseQuickSearch(input)).toThrow(QuickSearchSyntaxError);
  });

  it.each(["comp:", "assignee:", "assignee:@", "reporter:@"])(
    "rejects an empty recognized qualifier %s",
    (input) => {
      expect(() => parseQuickSearch(input)).toThrow(QuickSearchSyntaxError);
    },
  );

  it("rejects a bare empty assignee handle", () => {
    expect(() => parseQuickSearch("@")).toThrow(/requires an assignee handle/u);
  });

  it.each(['"unterminated', "dangling\\", '""'])(
    "rejects malformed tokenization in %s",
    (input) => {
      expect(() => parseQuickSearch(input)).toThrow(QuickSearchSyntaxError);
    },
  );
});
