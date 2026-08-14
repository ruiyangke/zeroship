import { readFileSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * Reads go through the query cache. Every one of them.
 *
 * `src/lib/queries.ts` states the rule -- no component calls a procedure
 * outside a hook there -- and the rule held only because someone kept
 * auditing by hand. That is the same fragility as the RPC auth policy, which
 * claimed every procedure was listed explicitly and had quietly lost
 * `groups.delete`: a convention nothing enforces decays at the speed people
 * forget it.
 *
 * WHY IT MATTERS, concretely. A component that fetches outside the cache is
 * invisible to invalidation. A mutation drops a key, nothing is subscribed to
 * it, and the component keeps rendering the world as it was until it happens
 * to remount. That bug shipped in the header's unread badge: marking a
 * notification read updated the dashboard and left the badge showing the old
 * count, because `UnreadBadge` was a `useState` + `useEffect` and the key it
 * should have been reading had no reader.
 *
 * HOW THIS CHECKS, and why not by grepping call sites. The obvious version
 * greps for `await listX(` in components. I ran exactly that audit by hand and
 * it reported a clean zero while THREE dead read imports and one live
 * imperative call were sitting in the tree -- a grep answers how a call is
 * spelled, not what it is. So this reads the authoritative classification
 * instead: `src/index.ts` declares each procedure as `query` or as
 * `mutation`/`action`/`stream`, and a component may import the second kind
 * (it hands them to `useAppMutation`) but not the first.
 */

const root = process.cwd();
const index = readFileSync(resolve(root, "src/index.ts"), "utf8");

/** Procedures declared `query(...)` -- the reads. */
function readProcedures(): Set<string> {
  return new Set(
    [...index.matchAll(/export const ([A-Za-z0-9_]+) = query\(/g)].map((m) => m[1]),
  );
}

/** Procedures declared `mutation`/`action`/`stream` -- legitimate to import. */
function writeProcedures(): Set<string> {
  return new Set(
    [...index.matchAll(/export const ([A-Za-z0-9_]+) = (?:mutation|action|stream)\(/g)].map(
      (m) => m[1],
    ),
  );
}

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  const walk = (d: string) => {
    for (const entry of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, entry.name);
      if (entry.isDirectory()) walk(p);
      else if (/\.tsx?$/.test(entry.name)) out.push(p);
    }
  };
  walk(resolve(root, dir));
  return out;
}

/** Named imports a file takes from the api barrel. */
function apiImports(source: string): string[] {
  const m = source.match(/import\s*\{([^}]*)\}\s*from\s*"[^"]*\/api"/s);
  if (!m) return [];
  return m[1]
    .split(",")
    .map((x) => x.trim().split(/\s+as\s+/)[0].trim())
    .filter(Boolean);
}

/**
 * The one read a component may hold directly, with its reason.
 *
 * `structuredSearch` is a query by declaration but imperative in use: the
 * advanced builder runs it on demand and the CALLER carries the rows away to
 * render as results. There is no key it belongs under, because nothing else
 * ever wants that answer -- caching it would mean inventing an owner for a
 * result that has none. It runs through `useAppMutation` so its error and
 * pending states still come from one place.
 *
 * An entry here is a claim that a read has no cache identity. Adding one
 * should feel like it needs an argument, which is why they carry a reason.
 */
const ALLOWED: Record<string, string> = {
  structuredSearch: "imperative on-demand search; the caller owns the rows, no cache identity",
};

describe("one cache for server state", () => {
  it("finds the procedure declarations at all", () => {
    // Guard. If these regexes stop matching, every assertion below passes
    // against two empty sets and the file becomes decoration.
    expect(readProcedures().size, "no query() procedures parsed from src/index.ts").toBeGreaterThan(20);
    expect(writeProcedures().size, "no mutation() procedures parsed from src/index.ts").toBeGreaterThan(20);
    expect(sourceFiles("src/components").length + sourceFiles("src/pages").length).toBeGreaterThan(20);
  });

  it("no component or page imports a read procedure", () => {
    const reads = readProcedures();
    const offenders: string[] = [];

    for (const dir of ["src/components", "src/pages"]) {
      for (const file of sourceFiles(dir)) {
        const imported = apiImports(readFileSync(file, "utf8"));
        const bad = imported.filter((name) => reads.has(name) && !(name in ALLOWED));
        if (bad.length > 0) {
          offenders.push(`${file.replace(`${root}/`, "")}: ${bad.join(", ")}`);
        }
      }
    }

    expect(
      offenders,
      "these read procedures are imported outside src/lib/queries.ts, so whatever they fetch is " +
        "invisible to invalidation -- add a hook in queries.ts and read through it, or if the read " +
        "genuinely has no cache identity, add it to ALLOWED with the reason:\n  " +
        offenders.join("\n  "),
    ).toEqual([]);
  });

  it("keeps the allowlist honest", () => {
    // An allowlist entry for a procedure that no longer exists, or that is not
    // actually a read, is a stale exemption -- it would silently permit a
    // future procedure that happened to reuse the name.
    const reads = readProcedures();
    for (const name of Object.keys(ALLOWED)) {
      expect(reads.has(name), `ALLOWED names "${name}", which is not a query procedure`).toBe(true);
    }
  });
});
