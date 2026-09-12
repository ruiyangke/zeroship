import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { WorkflowPublisher } from "../src/workflow-publisher.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(accept => { resolve = accept; });
  return { promise, resolve };
}

test("source changes during a build discard its image before publication", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "workflows.zship");
  const started = deferred<void>();
  const stale = deferred<void>();
  const seen: string[][] = [];
  let builds = 0;
  const publisher = new WorkflowPublisher(path, async () => {
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
    assert.deepEqual(await fs.readdir(root), ["workflows.zship"]);
  } finally {
    await publisher.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("build failure preserves the archive and a correction can publish", async () => {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-workflow-publisher-"));
  const path = join(root, "workflows.zship");
  let fail = true;
  const publisher = new WorkflowPublisher(path, async () => {
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
  const path = join(root, "workflows.zship");
  const pending = deferred<void>();
  const publisher = new WorkflowPublisher(path, async () => {
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
