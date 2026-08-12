import { describe, expect, it } from "vitest";
import {
  weaklyConnectedComponent,
  wouldCreateDirectedCycle,
  type DirectedEdge,
} from "../src/lib/graph";

describe("directed graph helpers", () => {
  it("rejects a self-edge even in an empty graph", () => {
    expect(wouldCreateDirectedCycle([], "bug_a", "bug_a")).toBe(true);
  });

  it("allows an ordinary edge in an empty graph", () => {
    expect(wouldCreateDirectedCycle([], "bug_a", "bug_b")).toBe(false);
  });

  it("detects a direct two-node cycle", () => {
    const edges = [{ from: "bug_a", to: "bug_b" }];
    expect(wouldCreateDirectedCycle(edges, "bug_b", "bug_a")).toBe(true);
  });

  it("detects a transitive cycle", () => {
    const edges = [
      { from: "bug_a", to: "bug_b" },
      { from: "bug_b", to: "bug_c" },
    ];
    expect(wouldCreateDirectedCycle(edges, "bug_c", "bug_a")).toBe(true);
  });

  it("gets the dependency direction right for a redundant forward edge", () => {
    const edges = [
      { from: "bug_a", to: "bug_b" },
      { from: "bug_b", to: "bug_c" },
    ];
    expect(wouldCreateDirectedCycle(edges, "bug_a", "bug_c")).toBe(false);
  });

  it("finds a cycle through one branch", () => {
    const edges = [
      { from: "bug_a", to: "bug_b" },
      { from: "bug_a", to: "bug_c" },
      { from: "bug_c", to: "bug_d" },
    ];
    expect(wouldCreateDirectedCycle(edges, "bug_d", "bug_a")).toBe(true);
  });

  it("terminates when unrelated input already contains a cycle", () => {
    const edges = [
      { from: "bug_x", to: "bug_y" },
      { from: "bug_y", to: "bug_x" },
    ];
    expect(wouldCreateDirectedCycle(edges, "bug_a", "bug_b")).toBe(false);
  });

  it("consumes any edge iterable only once", () => {
    function* edges(): Generator<DirectedEdge> {
      yield { from: "bug_a", to: "bug_b" };
      yield { from: "bug_b", to: "bug_c" };
    }
    expect(wouldCreateDirectedCycle(edges(), "bug_c", "bug_a")).toBe(true);
  });
});

describe("duplicate clusters", () => {
  it("finds canonical bugs, direct duplicates, and duplicate descendants", () => {
    const edges = [
      { from: "bug_a", to: "bug_c" },
      { from: "bug_b", to: "bug_c" },
      { from: "bug_d", to: "bug_b" },
      { from: "bug_x", to: "bug_y" },
    ];
    expect(weaklyConnectedComponent(edges, "bug_c")).toEqual([
      "bug_a",
      "bug_b",
      "bug_c",
      "bug_d",
    ]);
  });

  it("returns a singleton for a bug with no duplicate links", () => {
    expect(weaklyConnectedComponent([], "bug_lonely")).toEqual(["bug_lonely"]);
  });

  it("terminates and deduplicates nodes if handed a dirty cyclic graph", () => {
    const edges = [
      { from: "bug_a", to: "bug_b" },
      { from: "bug_b", to: "bug_c" },
      { from: "bug_c", to: "bug_a" },
    ];
    expect(weaklyConnectedComponent(edges, "bug_b")).toEqual([
      "bug_a",
      "bug_b",
      "bug_c",
    ]);
  });
});
