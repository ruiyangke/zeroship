import assert from "node:assert/strict";
import { rmSync } from "node:fs";
import { spawn } from "node:child_process";
import net from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const exampleRoot = resolve(__dirname, "..");
const repoRoot = resolve(exampleRoot, "..", "..");

let checks = 0;
const serverLog = [];
const rpcIds = {
  health: "db-e2e.health",
  seedDemo: "db-e2e.seed-demo",
  createTask: "db-e2e.create-task",
  upsertWorkspace: "db-e2e.upsert-workspace",
  getTask: "db-e2e.get-task",
  queryShowcase: "db-e2e.query-showcase",
  tasksWithRelations: "db-e2e.tasks-with-relations",
  updateTaskVersioned: "db-e2e.update-task-versioned",
  softDeleteTask: "db-e2e.soft-delete-task",
  restoreTask: "db-e2e.restore-task",
  purgeTask: "db-e2e.purge-task",
  transactionShowcase: "db-e2e.transaction-showcase",
  searchShowcase: "db-e2e.search-showcase",
  securityShowcase: "db-e2e.security-showcase",
  liveTasks: "db-e2e.live-tasks",
};

function recordCheck(name) {
  checks += 1;
  console.log(`✓ ${name}`);
}

function check(name, predicate, detail) {
  assert.ok(predicate, detail ?? name);
  recordCheck(name);
}

function eq(name, actual, expected) {
  assert.deepEqual(actual, expected);
  recordCheck(name);
}

function includesAll(name, actual, expected) {
  for (const item of expected) {
    assert.ok(actual.includes(item), `${name}: missing ${item}; got ${JSON.stringify(actual)}`);
  }
  recordCheck(name);
}

async function getFreePort() {
  return await new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        reject(new Error("failed to allocate port"));
        return;
      }
      const { port } = address;
      server.close((error) => {
        if (error) reject(error);
        else resolvePort(port);
      });
    });
  });
}

function sleep(ms) {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
}

async function withTimeout(promise, ms, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function waitForFrame(stream, label, predicate, timeoutMs = 5000) {
  let lastValue;
  return await withTimeout((async () => {
    while (true) {
      const value = await stream.nextValue();
      lastValue = value;
      if (predicate(value)) return value;
    }
  })(), timeoutMs, `${label} (last=${JSON.stringify(lastValue)})`);
}

function rpcBody(input) {
  return JSON.stringify({ json: input ?? {} });
}

async function rpc(baseUrl, name, input = {}) {
  const response = await fetch(`${baseUrl}/__zeroship/v1/${name}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: rpcBody(input),
  });
  const text = await response.text();
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    throw new Error(`rpc ${name} returned non-JSON (${response.status}): ${text}`);
  }
  if (!response.ok) {
    throw new Error(`rpc ${name} failed (${response.status}): ${JSON.stringify(parsed)}`);
  }
  if (!("json" in parsed)) {
    throw new Error(`rpc ${name} missing json envelope: ${JSON.stringify(parsed)}`);
  }
  return parsed.json;
}

function encodeStreamInput(input) {
  return Buffer.from(JSON.stringify({ json: input })).toString("base64")
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/g, "");
}

class LineValueStream {
  constructor(response) {
    this.reader = response.body.getReader();
    this.decoder = new TextDecoder();
    this.buffer = "";
  }

  _shiftLine() {
    const idx = this.buffer.indexOf("\n");
    if (idx === -1) return null;
    const line = this.buffer.slice(0, idx).replace(/\r$/, "");
    this.buffer = this.buffer.slice(idx + 1);
    return line;
  }

  async nextValue() {
    while (true) {
      const line = this._shiftLine();
      if (line !== null) {
        if (line.length === 0) continue;
        if (line.startsWith("2:")) {
          const payload = JSON.parse(line.slice(2));
          if (Array.isArray(payload) && payload.length > 0) return payload[0];
          throw new Error(`stream emitted malformed data frame: ${line}`);
        }
        if (line.startsWith("d:")) throw new Error("stream terminated before next value");
        continue;
      }
      const { value, done } = await this.reader.read();
      if (done) throw new Error("stream ended before emitting the next value");
      this.buffer += this.decoder.decode(value, { stream: true });
    }
  }

  async close() {
    await this.reader.cancel();
  }
}

async function openValueStream(baseUrl, name, input) {
  const response = await fetch(
    `${baseUrl}/__zeroship/v1/${name}?input=${encodeStreamInput(input)}`,
    { headers: { Accept: "text/event-stream" } },
  );
  if (!response.ok || !response.body) {
    const text = await response.text();
    throw new Error(`stream ${name} failed to open (${response.status}): ${text}`);
  }
  return new LineValueStream(response);
}

async function waitForHealthy(baseUrl) {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const health = await rpc(baseUrl, rpcIds.health, {});
      if (health?.ok === true) return;
    } catch {
      // keep polling while the server starts
    }
    await sleep(250);
  }
  throw new Error("zeroship serve did not become healthy");
}

async function main() {
  rmSync(resolve(exampleRoot, ".zeroship"), { recursive: true, force: true });

  const vitePort = Number(process.env.DB_E2E_VITE_PORT ?? await getFreePort());
  const apiPort = Number(process.env.DB_E2E_API_PORT ?? await getFreePort());
  const baseUrl = `http://127.0.0.1:${apiPort}`;

  const child = spawn(
    "pnpm",
    ["dev", "--host", "127.0.0.1", "--port", String(vitePort), "--strictPort"],
    {
      cwd: exampleRoot,
      env: {
        ...process.env,
        DB_E2E_API_PORT: String(apiPort),
        ZEROSHIP_BIN: resolve(repoRoot, "target/debug/zeroship"),
        ZEROSHIP_COLUMN_KEY_DB_E2E: "a".repeat(64),
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  child.stdout.on("data", (chunk) => {
    serverLog.push(String(chunk));
  });
  child.stderr.on("data", (chunk) => {
    serverLog.push(String(chunk));
  });

  try {
    await waitForHealthy(baseUrl);
    recordCheck("server became healthy");

    const seeded = await rpc(baseUrl, rpcIds.seedDemo);
    eq("seeded alpha slug", seeded.workspaces.alpha.slug, "alpha");
    eq("seeded beta tier", seeded.workspaces.beta.tier, "free");
    eq("insert system version starts at 1", seeded.tasks.launch.version, 1);
    eq("insert created_by is null on sqlite dev runtime", seeded.tasks.launch.created_by, null);
    eq("insert updated_by is null on sqlite dev runtime", seeded.tasks.launch.updated_by, null);
    check(
      "insert timestamps are populated",
      typeof seeded.tasks.launch.created_at === "number" &&
      typeof seeded.tasks.launch.updated_at === "number" &&
      seeded.tasks.launch.updated_at >= seeded.tasks.launch.created_at,
    );
    eq("masked contact email is returned by default", seeded.users.alice.contactEmail, "a***@alpha.test");
    eq("masked ssn is returned by default", seeded.users.alice.ssn, "***-**-6789");

    const upsertExisting = await rpc(baseUrl, rpcIds.upsertWorkspace, {
      slug: "alpha",
      name: "Alpha Workspace",
      tier: "enterprise",
      region: "us-west-1",
    });
    eq("upsert conflict updates the existing row", upsertExisting.tier, "enterprise");
    eq("upsert kept the alpha slug", upsertExisting.slug, "alpha");

    const upsertInserted = await rpc(baseUrl, rpcIds.upsertWorkspace, {
      slug: "gamma",
      name: "Gamma Workspace",
      tier: "pro",
      region: "eu-west-1",
    });
    eq("upsert can insert a new row", upsertInserted.slug, "gamma");
    eq("upsert inserted row starts at version 1", upsertInserted.version, 1);

    const queryShowcase = await rpc(baseUrl, rpcIds.queryShowcase, {
      workspaceId: seeded.workspaces.alpha.id,
      taskId: seeded.tasks.launch.id,
      afterId: seeded.tasks.triage.id,
      handle: "alice",
      primaryOwnerId: seeded.users.alice.id,
      secondaryOwnerId: seeded.users.bob.id,
    });
    eq("get by id returns the requested task", queryShowcase.byId.id, seeded.tasks.launch.id);
    eq("get by filter returns the triage task", queryShowcase.byFilter.title, "Alpha bug triage");
    eq("Query.first returns the highest-priority open task", queryShowcase.firstOpen.title, "Alpha analytics setup");
    eq("Query.unique returns the requested user", queryShowcase.uniqueUser.handle, "alice");
    eq("query filter + sort + skip + limit yields two rows", queryShowcase.filtered.length, 2);
    eq("filtered window is sorted after skip", queryShowcase.filtered[0].title, "Alpha launch plan");
    eq("after cursor pagination returns two rows", queryShowcase.afterPage.length, 2);
    eq("after cursor starts after the provided id", queryShowcase.afterPage[0].title, "Alpha docs cleanup");
    eq("count excludes done rows in the alpha workspace", queryShowcase.countActive, 4);
    includesAll("distinct statuses include the seeded set", queryShowcase.distinctStatuses, ["done", "in_progress", "open"]);
    eq("aggregate groups all alpha task statuses", queryShowcase.aggregate.length, 3);
    check(
      "aggregate includes the open group",
      queryShowcase.aggregate.some((row) => row.status === "open" && row.count >= 3),
    );

    const related = await rpc(baseUrl, rpcIds.tasksWithRelations, {
      workspaceId: seeded.workspaces.alpha.id,
    });
    eq("joined task rows preserve relation count", related.length, 5);
    eq("with() eager-loaded the owner row", related[0].owner.handle, "bob");
    eq("with() eager-loaded the workspace row", related[0].workspace.slug, "alpha");

    const created = await rpc(baseUrl, rpcIds.createTask, {
      workspaceId: seeded.workspaces.alpha.id,
      ownerId: seeded.users.alice.id,
      title: "Inserted through createTask",
      description: "Exercise single-row insert over RPC",
      status: "open",
      priority: 5,
      score: 88,
      category: "feature",
      tags: ["insert", "rpc"],
    });
    eq("single insert returns requested title", created.title, "Inserted through createTask");
    eq("single insert version starts at 1", created.version, 1);

    await sleep(20);
    const updated = await rpc(baseUrl, rpcIds.updateTaskVersioned, {
      id: created.id,
      version: created.version,
      title: "Updated through optimistic concurrency",
    });
    eq("versioned update succeeds", updated.ok, true);
    eq("update changed the title", updated.task.title, "Updated through optimistic concurrency");
    eq("update increments version", updated.task.version, 2);
    check(
      "update advances updated_at",
      updated.task.updated_at >= created.updated_at,
    );

    const stale = await rpc(baseUrl, rpcIds.updateTaskVersioned, {
      id: created.id,
      version: created.version,
      title: "This stale update should fail",
    });
    eq("stale update returns a typed VERSION_MISMATCH", stale.code, "VERSION_MISMATCH");

    const softDeleted = await rpc(baseUrl, rpcIds.softDeleteTask, { id: created.id });
    check("soft delete stamps deleted_at", typeof softDeleted.deleted.deleted_at === "number");
    eq("soft delete hides the row from get()", softDeleted.visibleAfterDelete, null);
    eq("soft delete hides the row from count()", softDeleted.countAfterDelete, 0);

    const restored = await rpc(baseUrl, rpcIds.restoreTask, { id: created.id });
    eq("restore clears deleted_at", restored.restored.deleted_at, null);
    eq("restore makes the row visible again", restored.countAfterRestore, 1);

    const purged = await rpc(baseUrl, rpcIds.purgeTask, { id: created.id });
    eq("purge removes the row permanently", purged.visibleAfterPurge, null);
    eq("purge leaves no visible row", purged.countAfterPurge, 0);

    const tx = await rpc(baseUrl, rpcIds.transactionShowcase, {
      workspaceId: seeded.workspaces.alpha.id,
      ownerId: seeded.users.alice.id,
    });
    eq("transaction commit returns the created row", tx.commit.created.title, "Tx commit task");
    eq("transaction rollback surfaces the expected code", tx.rollback.errorCode, "EXPECTED_ROLLBACK");
    eq("rolled-back rows are not persisted", tx.rollback.visibleCount, 0);
    eq("nested savepoint isolates the inner failure", tx.nested.innerErrorCode, "INNER_ABORT");
    eq("outer transaction row committed", tx.nested.outerCount, 1);
    eq("inner transaction row rolled back", tx.nested.innerCount, 0);

    const search = await rpc(baseUrl, rpcIds.searchShowcase, {
      workspaceId: seeded.workspaces.alpha.id,
    });
    includesAll("FTS membership includes the coffee places", search.text.map((row) => row.name).sort(), ["Alpha Cafe", "Alpha HQ"]);
    includesAll("vector search membership includes the nearest embeddings", search.vector.map((row) => row.name).sort(), ["Alpha Cafe", "Alpha HQ"]);
    includesAll("geo near membership includes only nearby San Francisco places", search.near.map((row) => row.name).sort(), ["Alpha Cafe", "Alpha HQ"]);
    check(
      "geo near excludes the Oakland warehouse at 1.5km",
      !search.near.some((row) => row.name === "Alpha Warehouse"),
    );

    const security = await rpc(baseUrl, rpcIds.securityShowcase, {
      userId: seeded.users.alice.id,
    });
    eq("MaskedValue.canUnmask allows support", security.can.support, true);
    eq("MaskedValue.canUnmask denies unknown roles", security.can.guest, false);
    eq("unauthorized unmask returns the typed denial code", security.deniedCode, "unmask_not_permitted");
    eq("single-column unmask returns plaintext", security.plainSsn, "123-45-6789");
    eq("row-scoped multi-column unmask returns plaintext", security.rowReveal.contactEmail, "alice.private@alpha.test");
    eq("bulkUnmask returns plaintext by row id", security.bulk[seeded.users.alice.id].ssn, "123-45-6789");
    eq("per-query unmask hints reveal the hinted column", security.hinted.ssn, "123-45-6789");
    eq("per-query unmask leaves other masked columns masked", security.hinted.contactEmail, "a***@alpha.test");

    const live = await openValueStream(baseUrl, rpcIds.liveTasks, {
      workspaceId: seeded.workspaces.alpha.id,
    });
    try {
      const initialFrame = await withTimeout(live.nextValue(), 5000, "initial live frame");
      const initialCount = initialFrame.length;
      check("db.live yields the initial task snapshot", initialCount >= 7);

      const liveCreated = await rpc(baseUrl, rpcIds.createTask, {
        workspaceId: seeded.workspaces.alpha.id,
        ownerId: seeded.users.bob.id,
        title: "Live task inserted",
        description: "Should trigger db.live on insert",
        status: "open",
        priority: 3,
        score: 55,
        category: "live",
        tags: ["live", "insert"],
      });
      const afterInsert = await waitForFrame(
        live,
        "live insert frame",
        (rows) =>
          rows.length === initialCount + 1 &&
          rows.some((row) => row.id === liveCreated.id && row.title === "Live task inserted"),
      );
      eq("db.live reacts to inserts", afterInsert.length, initialCount + 1);
      check(
        "insert frame contains the new title",
        afterInsert.some((row) => row.id === liveCreated.id && row.title === "Live task inserted"),
      );

      const liveUpdated = await rpc(baseUrl, rpcIds.updateTaskVersioned, {
        id: liveCreated.id,
        version: liveCreated.version,
        title: "Live task updated",
      });
      eq("live update uses optimistic concurrency successfully", liveUpdated.ok, true);
      const afterUpdate = await waitForFrame(
        live,
        "live update frame",
        (rows) => rows.some((row) => row.id === liveCreated.id && row.title === "Live task updated"),
      );
      check(
        "db.live reacts to updates",
        afterUpdate.some((row) => row.id === liveCreated.id && row.title === "Live task updated"),
      );

      await rpc(baseUrl, rpcIds.softDeleteTask, { id: liveCreated.id });
      const afterDelete = await waitForFrame(
        live,
        "live delete frame",
        (rows) => rows.length === initialCount && !rows.some((row) => row.id === liveCreated.id),
      );
      eq("db.live reacts to soft deletes by shrinking the visible result", afterDelete.length, initialCount);
      check(
        "deleted task is removed from the live result",
        !afterDelete.some((row) => row.id === liveCreated.id),
      );
    } finally {
      await live.close();
    }

    console.log(`PASS ${checks} checks`);
  } catch (error) {
    console.error("\nServer log:");
    console.error(serverLog.join(""));
    throw error;
  } finally {
    child.kill("SIGTERM");
    await new Promise((resolveExit) => child.once("exit", () => resolveExit(undefined)));
  }
}

main().catch((error) => {
  console.error(error.stack ?? String(error));
  process.exit(1);
});
