import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { DevPublisher } from "../src/dev-publisher.js";
import { WorkerBuildError } from "../src/build.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(accept => { resolve = accept; });
  return { promise, resolve };
}

test("source changes during a build discard its image before publication", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "app.zship");
  const started = deferred<void>();
  const stale = deferred<void>();
  const seen: string[][] = [];
  let builds = 0;
  const publisher = new DevPublisher(path, async () => {
    builds += 1;
    if (builds === 1) {
      started.resolve();
      await stale.promise;
      return { archive: Buffer.from("stale"), dependencies: ["old.ts"] };
    }
    assert.equal(await fs.readFile(path, "utf8"), "retained");
    return { archive: Buffer.from("current"), dependencies: ["new.ts"] };
  }, files => { seen.push(files); });
  try {
    await fs.writeFile(path, "retained");
    const first = publisher.refresh();
    await started.promise;
    const changed = publisher.refresh();
    stale.resolve();
    await Promise.all([first, changed]);
    assert.equal(builds, 2);
    assert.equal(await fs.readFile(path, "utf8"), "current");
    assert.deepEqual(seen.at(-1), ["new.ts"]);
    assert.deepEqual(await fs.readdir(root), ["app.zship"]);
  } finally {
    await publisher.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("build failure preserves the archive and a correction can publish", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "app.zship");
  let fail = true;
  const publisher = new DevPublisher(path, async () => {
    if (fail) throw new Error("invalid source");
    return { archive: Buffer.from("corrected"), dependencies: [] };
  }, () => {});
  try {
    await fs.writeFile(path, "retained");
    await assert.rejects(publisher.refresh(), /invalid source/);
    assert.equal(await fs.readFile(path, "utf8"), "retained");
    fail = false;
    await publisher.refresh();
    assert.equal(await fs.readFile(path, "utf8"), "corrected");
  } finally {
    await publisher.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("closing during a build prevents publication and waits for cleanup", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "app.zship");
  const pending = deferred<void>();
  const publisher = new DevPublisher(path, async () => {
    await pending.promise;
    return { archive: Buffer.from("late"), dependencies: [] };
  }, () => {});
  try {
    const refresh = publisher.refresh();
    const closed = publisher.close();
    pending.resolve();
    await Promise.all([refresh, closed]);
    assert.deepEqual(await fs.readdir(root), []);
    await assert.rejects(publisher.refresh(), /closed/);
  } finally {
    await publisher.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a failed worker build reports the sources it read", async () => {
  // The caller watches what the last build read, so the source that broke a
  // build is watched until it is fixed.
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "app.zship");
  const results: Array<() => { archive: Buffer; dependencies: string[] }> = [
    () => ({ archive: Buffer.from("first"), dependencies: ["entry.ts"] }),
    () => { throw new WorkerBuildError(new Error("broken catalog"), ["entry.ts", "messages.po"]); },
    () => { throw new Error("not a worker build"); },
  ];
  const seen: string[][] = [];
  const publisher = new DevPublisher(path, async () => results.shift()!(), (files) => {
    seen.push(files);
  });
  try {
    await publisher.refresh();
    await assert.rejects(publisher.refresh(), /broken catalog/);
    await assert.rejects(publisher.refresh(), /not a worker build/);
    assert.deepEqual(seen, [["entry.ts"], ["entry.ts", "messages.po"]]);
    assert.equal(await fs.readFile(path, "utf8"), "first");
  } finally {
    await publisher.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});
