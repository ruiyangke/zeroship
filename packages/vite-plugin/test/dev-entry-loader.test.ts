import { describe, test } from "node:test";
import assert from "node:assert/strict";

import type { HmrUpdate } from "../src/dev-bootstrap/hmr.js";
import { createEntryLoader, type EntryLoaderHost } from "../src/dev-bootstrap/loader.js";

const ENTRY = "/app/src/server.ts";

/**
 * A dev server's change queue and a runner whose entry import stays open until
 * the test releases it, so the test decides what is queued before the first
 * load, what is queued while it runs, and when the poll interval fires.
 */
function loaderHost(options: { unreachable?: boolean } = {}) {
  const queue: string[] = [];
  const taken: string[][] = [];
  const warnings: string[] = [];
  let onChange: ((update: HmrUpdate) => void) | undefined;
  let importStarted!: () => void;
  const started = new Promise<void>((resolve) => { importStarted = resolve; });
  let releaseImport!: () => void;
  const released = new Promise<void>((resolve) => { releaseImport = resolve; });

  const take = (): HmrUpdate => {
    const changed = queue.splice(0);
    taken.push(changed);
    return { changed, bindingsVersion: "bindings-1" };
  };
  const host: EntryLoaderHost = {
    entry: ENTRY,
    takeChanges: async () => {
      if (options.unreachable) throw new Error("connect ECONNREFUSED");
      return take();
    },
    watchChanges(listener) {
      onChange = listener;
    },
    readBindings: async () => ({ version: "bindings-1", bindings: [] }),
    runner: async () => ({
      async import(id: string) {
        assert.equal(id, ENTRY);
        importStarted();
        await released;
        return { default: { fetch: () => "entry" } };
      },
      evaluatedModules: {
        getModuleById: () => undefined,
        getModulesByFile: () => undefined,
        invalidateModule() {},
      },
    }),
    resetRunner() {},
    warn(message) {
      warnings.push(message);
    },
  };
  return {
    host,
    queue,
    /** Every batch the loader took from the queue, in order. */
    taken,
    warnings,
    started,
    releaseImport,
    /** One firing of the poll interval: it takes whatever is queued. */
    tick() {
      assert.ok(onChange, "the loader must be watching for changes by now");
      onChange(take());
    },
  };
}

describe("native dev entry loader", () => {
  test("changes queued before the first load do not invalidate it", async () => {
    const dev = loaderHost();
    let invalidations = 0;
    const load = createEntryLoader(dev.host, () => { invalidations += 1; });

    // Queued before any load began: the edit that got this runtime spawned.
    dev.queue.push("/app/src/value.ts");
    const loaded = load();
    await dev.started;
    dev.tick();
    dev.releaseImport();
    const entry = await loaded as { fetch?: () => unknown };

    assert.equal(invalidations, 0);
    // The queued change existed, and it was the load's own first take that
    // consumed it, not the poll that fired during the load.
    assert.deepEqual(dev.taken, [["/app/src/value.ts"], []]);
    assert.equal(entry.fetch?.(), "entry");
  });

  test("a change queued while the first load runs invalidates it", async () => {
    const dev = loaderHost();
    let invalidations = 0;
    const load = createEntryLoader(dev.host, () => { invalidations += 1; });

    const loaded = load();
    await dev.started;
    dev.queue.push("/app/src/value.ts");
    dev.tick();
    dev.releaseImport();
    await loaded;

    assert.equal(invalidations, 1);
    assert.deepEqual(dev.taken, [[], ["/app/src/value.ts"]]);
    assert.deepEqual(dev.warnings, []);
  });

  test("a queue that cannot be cleared is reported, and changes are still watched", async () => {
    const dev = loaderHost({ unreachable: true });
    let invalidations = 0;
    const load = createEntryLoader(dev.host, () => { invalidations += 1; });

    const loaded = load();
    await dev.started;
    dev.releaseImport();
    const entry = await loaded as { fetch?: () => unknown };
    assert.equal(entry.fetch?.(), "entry");
    assert.equal(dev.warnings.length, 1);
    assert.match(dev.warnings[0]!, /could not clear the dev server's change queue.*ECONNREFUSED/);

    dev.queue.push("/app/src/value.ts");
    dev.tick();
    assert.equal(invalidations, 1);
  });
});
