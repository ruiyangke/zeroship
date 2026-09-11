import { expect, test } from "vitest";
import { setTimeout as sleep } from "node:timers/promises";
import { rpc, openValueStream, withTimeout, waitForFrame } from "./rpc";
import { target } from "./target";

function check(name: string, predicate: boolean) { expect(predicate, name).toBe(true); }
function eq(name: string, actual: unknown, expected: unknown) { expect(actual, name).toEqual(expected); }
function includesAll(name: string, actual: unknown[], expected: unknown[]) {
  expect(actual, name).toEqual(expect.arrayContaining(expected));
}

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
} as const;

test("SQLite CRUD, queries, transactions, protection and committed live changes", async () => {
  const baseUrl = target().apiUrl;
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

});
