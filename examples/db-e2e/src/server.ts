"use server";

import {
  defineMaskPolicy,
  schema,
  t,
  type Db,
  type MaskedValue,
  type Result,
} from "@zeroship/db";
import { env } from "zeroship";
import { action, stream } from "@zeroship/rpc/server";

defineMaskPolicy({
  admin: ["public", "pii", "spi", "phi", "pci", "internal"],
  support: ["public", "pii", "spi"],
  auto: ["public", "pii", "spi", "phi", "pci", "internal"],
});

const dbSchema = {
  workspaces: schema({
    slug: t.string().required().unique().pattern(/^[a-z0-9-]+$/),
    name: t.string().required(),
    tier: t.string().enum("free", "pro", "enterprise").default("free"),
    region: t.string().required(),
  }).index("by_tier", ["tier"]),

  users: schema({
    workspaceId: t.ref("workspaces").required(),
    handle: t.string().required().unique().pattern(/^[a-z0-9_]+$/),
    fullName: t.string().required(),
    email: t.string().required().unique(),
    contactEmail: t.encrypted({
      mode: "deterministic",
      keyId: "db_e2e",
      wraps: t.string(),
    }).mask({ kind: "email", classification: "pii" }),
    ssn: t.encrypted({
      mode: "randomised",
      keyId: "db_e2e",
      wraps: t.string(),
    }).mask({ kind: "last4", classification: "spi" }),
    city: t.string().required(),
  }).index("by_workspace", ["workspaceId"]),

  tasks: schema({
    workspaceId: t.ref("workspaces").required(),
    ownerId: t.ref("users").required(),
    title: t.string().required().fts("english"),
    description: t.string().required().fts("english"),
    status: t.string().enum("open", "in_progress", "done", "archived").default("open"),
    priority: t.number().required(),
    score: t.number().required(),
    category: t.string().required(),
    tags: t.array(t.string()),
  })
    .index("by_workspace_status", ["workspaceId", "status"])
    .index("by_workspace_priority", ["workspaceId", "priority"]),

  places: schema({
    workspaceId: t.ref("workspaces").required(),
    name: t.string().required().fts("english"),
    description: t.string().required().fts("english"),
    category: t.string().required(),
    loc: t.geoPoint().required(),
    embedding: t.vector(4, { metric: "cosine" }),
    open: t.boolean().default(true),
  }).index("by_workspace_category", ["workspaceId", "category"]),
};

export default { schema: dbSchema };

const db = env.db as Db<typeof dbSchema>;

type WorkspaceId = typeof db.workspaces.Id;
type UserId = typeof db.users.Id;
type TaskId = typeof db.tasks.Id;

type SupportActor = { kind: "support"; id: string };
type AdminActor = { kind: "admin"; id: string };
type GuestActor = { kind: "guest"; id: string };

function expectData<T>(result: Result<T>, label: string): T {
  if (result.error) throw result.error;
  return result.data as T;
}

function expectSome<T>(value: T | null, label: string): T {
  if (value === null) throw new Error(`${label}: expected a row`);
  return value;
}

function summarizeWorkspace(row: Record<string, unknown>) {
  return {
    id: row.id,
    slug: row.slug,
    name: row.name,
    tier: row.tier,
    region: row.region,
    created_at: row.created_at,
    updated_at: row.updated_at,
    created_by: row.created_by,
    updated_by: row.updated_by,
    version: row.version,
    deleted_at: row.deleted_at,
  };
}

function summarizeUser(row: Record<string, unknown>) {
  return {
    id: row.id,
    workspaceId: row.workspaceId,
    handle: row.handle,
    fullName: row.fullName,
    email: row.email,
    contactEmail: String(row.contactEmail ?? ""),
    ssn: String(row.ssn ?? ""),
    city: row.city,
    created_at: row.created_at,
    updated_at: row.updated_at,
    created_by: row.created_by,
    updated_by: row.updated_by,
    version: row.version,
    deleted_at: row.deleted_at,
  };
}

function summarizeTask(row: Record<string, unknown>) {
  return {
    id: row.id,
    workspaceId: row.workspaceId,
    ownerId: row.ownerId,
    title: row.title,
    description: row.description,
    status: row.status,
    priority: row.priority,
    score: row.score,
    category: row.category,
    tags: row.tags,
    created_at: row.created_at,
    updated_at: row.updated_at,
    created_by: row.created_by,
    updated_by: row.updated_by,
    version: row.version,
    deleted_at: row.deleted_at,
  };
}

function summarizePlace(row: Record<string, unknown>) {
  return {
    id: row.id,
    workspaceId: row.workspaceId,
    name: row.name,
    description: row.description,
    category: row.category,
    loc: row.loc,
    open: row.open,
    created_at: row.created_at,
    updated_at: row.updated_at,
    version: row.version,
  };
}

function mapRows(
  rows: Array<Record<string, unknown>>,
  summary: (row: Record<string, unknown>) => Record<string, unknown>,
) {
  return rows.map((row) => summary(row));
}

function mapBulkUnmask(result: Map<string, Record<string, unknown>>) {
  return Object.fromEntries([...result.entries()]);
}

function maskedInsert(value: string): MaskedValue<string> {
  return value as unknown as MaskedValue<string>;
}

function supportActor(): SupportActor {
  return { kind: "support", id: "support_agent" };
}

function adminActor(): AdminActor {
  return { kind: "admin", id: "admin_agent" };
}

function guestActor(): GuestActor {
  return { kind: "guest", id: "guest_viewer" };
}

export const health = action(async (_input: Record<string, never>) => {
  return { ok: true, runtime: "zeroship", backend: "sqlite" };
}, { id: "db-e2e.health" });

export const seedDemo = action(async (_input: Record<string, never>) => {
  const [alpha, beta] = expectData(
    await db.workspaces.insertMany([
      { slug: "alpha", name: "Alpha Workspace", tier: "pro", region: "us-west-1" },
      { slug: "beta", name: "Beta Workspace", tier: "free", region: "us-east-1" },
    ]),
    "seedDemo.workspaces",
  ) as Array<Record<string, unknown>>;

  const alice = expectData(
    await db.users.insert({
      workspaceId: alpha.id as WorkspaceId,
      handle: "alice",
      fullName: "Alice A",
      email: "alice@alpha.test",
      contactEmail: maskedInsert("alice.private@alpha.test"),
      ssn: maskedInsert("123-45-6789"),
      city: "San Francisco",
    }),
    "seedDemo.alice",
  ) as Record<string, unknown>;

  const [bob, cara] = expectData(
    await db.users.insertMany([
      {
        workspaceId: alpha.id as WorkspaceId,
        handle: "bob",
        fullName: "Bob B",
        email: "bob@alpha.test",
        contactEmail: maskedInsert("bob.private@alpha.test"),
        ssn: maskedInsert("555-11-2222"),
        city: "Oakland",
      },
      {
        workspaceId: beta.id as WorkspaceId,
        handle: "cara",
        fullName: "Cara C",
        email: "cara@beta.test",
        contactEmail: maskedInsert("cara.private@beta.test"),
        ssn: maskedInsert("777-88-9999"),
        city: "Los Angeles",
      },
    ]),
    "seedDemo.users",
  ) as Array<Record<string, unknown>>;

  const [launch, triage, docs, analytics, roadmap] = expectData(
    await db.tasks.insertMany([
      {
        workspaceId: alpha.id as WorkspaceId,
        ownerId: alice.id as UserId,
        title: "Alpha launch plan",
        description: "Ship the launch checklist and announce the release",
        status: "open",
        priority: 3,
        score: 90,
        category: "feature",
        tags: ["launch", "marketing"],
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        ownerId: bob.id as UserId,
        title: "Alpha bug triage",
        description: "Investigate the login bug and patch the auth flow",
        status: "in_progress",
        priority: 2,
        score: 70,
        category: "bug",
        tags: ["auth", "bug"],
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        ownerId: alice.id as UserId,
        title: "Alpha docs cleanup",
        description: "Archive outdated onboarding docs and trim stale pages",
        status: "done",
        priority: 1,
        score: 40,
        category: "chore",
        tags: ["docs"],
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        ownerId: bob.id as UserId,
        title: "Alpha analytics setup",
        description: "Create the product dashboard and track signups",
        status: "open",
        priority: 4,
        score: 95,
        category: "feature",
        tags: ["analytics", "dashboard"],
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        ownerId: alice.id as UserId,
        title: "Alpha roadmap sync",
        description: "Review priorities for the next quarter and align stakeholders",
        status: "open",
        priority: 2,
        score: 65,
        category: "planning",
        tags: ["roadmap"],
      },
    ]),
    "seedDemo.tasks",
  ) as Array<Record<string, unknown>>;

  const [hq, cafe, warehouse, lab] = expectData(
    await db.places.insertMany([
      {
        workspaceId: alpha.id as WorkspaceId,
        name: "Alpha HQ",
        description: "Main office with coffee bar and customer lounge",
        category: "office",
        loc: { lat: 37.7749, lng: -122.4194 },
        embedding: [1, 0, 0, 0],
        open: true,
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        name: "Alpha Cafe",
        description: "Coffee and snacks for the office team",
        category: "cafe",
        loc: { lat: 37.7757, lng: -122.4188 },
        embedding: [0.96, 0.04, 0, 0],
        open: true,
      },
      {
        workspaceId: alpha.id as WorkspaceId,
        name: "Alpha Warehouse",
        description: "Storage and logistics hub for outbound shipments",
        category: "warehouse",
        loc: { lat: 37.8044, lng: -122.2712 },
        embedding: [0, 1, 0, 0],
        open: true,
      },
      {
        workspaceId: beta.id as WorkspaceId,
        name: "Beta Lab",
        description: "Research space for experiments and prototypes",
        category: "lab",
        loc: { lat: 34.0522, lng: -118.2437 },
        embedding: [0, 0, 1, 0],
        open: true,
      },
    ]),
    "seedDemo.places",
  ) as Array<Record<string, unknown>>;

  return {
    workspaces: {
      alpha: summarizeWorkspace(alpha),
      beta: summarizeWorkspace(beta),
    },
    users: {
      alice: summarizeUser(alice),
      bob: summarizeUser(bob),
      cara: summarizeUser(cara),
    },
    tasks: {
      launch: summarizeTask(launch),
      triage: summarizeTask(triage),
      docs: summarizeTask(docs),
      analytics: summarizeTask(analytics),
      roadmap: summarizeTask(roadmap),
    },
    places: {
      hq: summarizePlace(hq),
      cafe: summarizePlace(cafe),
      warehouse: summarizePlace(warehouse),
      lab: summarizePlace(lab),
    },
  };
}, { id: "db-e2e.seed-demo" });

export const createTask = action(async ({
  workspaceId,
  ownerId,
  title,
  description,
  status = "open",
  priority = 1,
  score = 50,
  category = "feature",
  tags = [],
}: {
  workspaceId: WorkspaceId;
  ownerId: UserId;
  title: string;
  description: string;
  status?: "open" | "in_progress" | "done" | "archived";
  priority?: number;
  score?: number;
  category?: string;
  tags?: string[];
}) => {
  const row = expectData(
    await db.tasks.insert({
      workspaceId,
      ownerId,
      title,
      description,
      status,
      priority,
      score,
      category,
      tags,
    }),
    "createTask",
  ) as Record<string, unknown>;
  return summarizeTask(row);
}, { id: "db-e2e.create-task" });

export const upsertWorkspace = action(async ({
  slug,
  name,
  tier,
  region,
}: {
  slug: string;
  name: string;
  tier: "free" | "pro" | "enterprise";
  region: string;
}) => {
  const row = expectData(
    await db.workspaces.upsert(
      { slug, name, tier, region },
      { conflictFields: ["slug"] },
    ),
    "upsertWorkspace",
  ) as Record<string, unknown>;
  return summarizeWorkspace(row);
}, { id: "db-e2e.upsert-workspace" });

export const getTask = action(async ({ id }: { id: TaskId }) => {
  const row = expectData(await db.tasks.get(id), "getTask") as Record<string, unknown> | null;
  return row ? summarizeTask(row) : null;
}, { id: "db-e2e.get-task" });

export const queryShowcase = action(async ({
  workspaceId,
  taskId,
  afterId,
  handle,
  primaryOwnerId,
  secondaryOwnerId,
}: {
  workspaceId: WorkspaceId;
  taskId: TaskId;
  afterId: TaskId;
  handle: string;
  primaryOwnerId: UserId;
  secondaryOwnerId: UserId;
}) => {
  const byId = expectSome(
    expectData(await db.tasks.get(taskId), "queryShowcase.byId") as Record<string, unknown> | null,
    "queryShowcase.byId",
  );
  const byFilter = expectSome(
    expectData(
      await db.tasks.get({ title: "Alpha bug triage" } as never),
      "queryShowcase.byFilter",
    ) as Record<string, unknown> | null,
    "queryShowcase.byFilter",
  );
  const firstOpen = expectSome(
    expectData(
      await db.tasks
        .find({ workspaceId, status: { $ne: "done" } } as never)
        .sort({ priority: -1, id: 1 })
        .first(),
      "queryShowcase.firstOpen",
    ) as Record<string, unknown> | null,
    "queryShowcase.firstOpen",
  );
  const uniqueUser = expectData(
    await db.users.find({ handle }).unique(),
    "queryShowcase.uniqueUser",
  ) as Record<string, unknown>;
  const filtered = expectData(
    await db.tasks
      .find({
        workspaceId,
        status: { $in: ["open", "in_progress"] },
        priority: { $gte: 2 },
        score: { $lte: 95 },
        category: { $nin: ["chore"] },
        title: { $ilike: "%alpha%" },
        $or: [{ ownerId: primaryOwnerId }, { ownerId: secondaryOwnerId }],
        $not: { status: "archived" },
      } as never)
      .sort({ priority: -1, title: 1 })
      .skip(1)
      .limit(2),
    "queryShowcase.filtered",
  ) as Array<Record<string, unknown>>;
  const afterPage = expectData(
    await db.tasks.find({ workspaceId }).sort({ id: 1 }).after(afterId).limit(2),
    "queryShowcase.afterPage",
  ) as Array<Record<string, unknown>>;
  const countActive = expectData(
    await db.tasks.count({ workspaceId, status: { $ne: "done" } } as never),
    "queryShowcase.countActive",
  ) as number;
  const distinctStatuses = expectData(
    await db.tasks.distinct("status", { workspaceId } as never),
    "queryShowcase.distinctStatuses",
  ) as Array<string | number | boolean | null>;
  const aggregate = expectData(
    await db.tasks.aggregate([
      { $match: { workspaceId } },
      {
        $group: {
          _id: "$status",
          count: { $count: true },
          totalScore: { $sum: "$score" },
          avgPriority: { $avg: "$priority" },
        },
      },
      { $sort: { totalScore: -1 } },
    ]),
    "queryShowcase.aggregate",
  );

  return {
    byId: summarizeTask(byId),
    byFilter: summarizeTask(byFilter),
    firstOpen: summarizeTask(firstOpen),
    uniqueUser: summarizeUser(uniqueUser),
    filtered: mapRows(filtered, summarizeTask),
    afterPage: mapRows(afterPage, summarizeTask),
    countActive,
    distinctStatuses,
    aggregate,
  };
}, { id: "db-e2e.query-showcase" });

export const tasksWithRelations = action(async ({
  workspaceId,
}: {
  workspaceId: WorkspaceId;
}) => {
  const rows = expectData(
    await db.tasks
      .find({ workspaceId })
      .sort({ title: 1 })
      .with({ ownerId: true, workspaceId: true }),
    "tasksWithRelations",
  ) as Array<Record<string, unknown>>;

  return rows.map((row) => {
    const owner = row.ownerId as Record<string, unknown> | null;
    const workspace = row.workspaceId as Record<string, unknown> | null;
    return {
      id: row.id,
      title: row.title,
      status: row.status,
      owner: owner ? { id: owner.id, handle: owner.handle, fullName: owner.fullName } : null,
      workspace: workspace ? { id: workspace.id, slug: workspace.slug, tier: workspace.tier } : null,
    };
  });
}, { id: "db-e2e.tasks-with-relations" });

export const updateTaskVersioned = action(async ({
  id,
  version,
  title,
}: {
  id: TaskId;
  version: number;
  title: string;
}) => {
  const result = await db.tasks.update({ id, version } as never, { title });
  if (result.error) {
    return {
      ok: false,
      code: (result.error as { code?: string }).code ?? null,
      name: result.error.name,
      message: result.error.message,
    };
  }
  return {
    ok: true,
    task: summarizeTask(result.data as Record<string, unknown>),
  };
}, { id: "db-e2e.update-task-versioned" });

export const softDeleteTask = action(async ({ id }: { id: TaskId }) => {
  const deleted = expectSome(
    expectData(await db.tasks.delete(id), "softDeleteTask.deleted") as Record<string, unknown> | null,
    "softDeleteTask.deleted",
  );
  const visibleAfterDelete = expectData(
    await db.tasks.get(id),
    "softDeleteTask.visibleAfterDelete",
  ) as Record<string, unknown> | null;
  const countAfterDelete = expectData(
    await db.tasks.count({ id } as never),
    "softDeleteTask.countAfterDelete",
  ) as number;
  return {
    deleted: summarizeTask(deleted),
    visibleAfterDelete: visibleAfterDelete ? summarizeTask(visibleAfterDelete) : null,
    countAfterDelete,
  };
}, { id: "db-e2e.soft-delete-task" });

export const restoreTask = action(async ({ id }: { id: TaskId }) => {
  const restored = expectSome(
    expectData(await db.tasks.restore(id), "restoreTask.restored") as Record<string, unknown> | null,
    "restoreTask.restored",
  );
  const countAfterRestore = expectData(
    await db.tasks.count({ id } as never),
    "restoreTask.countAfterRestore",
  ) as number;
  return {
    restored: summarizeTask(restored),
    countAfterRestore,
  };
}, { id: "db-e2e.restore-task" });

export const purgeTask = action(async ({ id }: { id: TaskId }) => {
  const purged = expectSome(
    expectData(await db.tasks.purge(id), "purgeTask.purged") as Record<string, unknown> | null,
    "purgeTask.purged",
  );
  const visibleAfterPurge = expectData(
    await db.tasks.get(id),
    "purgeTask.visibleAfterPurge",
  ) as Record<string, unknown> | null;
  const countAfterPurge = expectData(
    await db.tasks.count({ id } as never),
    "purgeTask.countAfterPurge",
  ) as number;
  return {
    purged: summarizeTask(purged),
    visibleAfterPurge: visibleAfterPurge ? summarizeTask(visibleAfterPurge) : null,
    countAfterPurge,
  };
}, { id: "db-e2e.purge-task" });

export const transactionShowcase = action(async ({
  workspaceId,
  ownerId,
}: {
  workspaceId: WorkspaceId;
  ownerId: UserId;
}) => {
  const commit = await db.transaction(async (tx) => {
    const created = await tx.tasks.insert({
      workspaceId,
      ownerId,
      title: "Tx commit task",
      description: "Created inside a committed transaction",
      status: "open",
      priority: 3,
      score: 77,
      category: "transaction",
      tags: ["tx", "commit"],
    });
    const found = await tx.tasks.find({ id: created.id } as never).unique();
    return {
      created: summarizeTask(created as unknown as Record<string, unknown>),
      found: summarizeTask(found as unknown as Record<string, unknown>),
    };
  }, { isolationLevel: "serializable" });
  if (commit.error) throw commit.error;

  const rollback = await db.transaction(async (tx) => {
    await tx.tasks.insert({
      workspaceId,
      ownerId,
      title: "Tx rollback task",
      description: "This row should never commit",
      status: "open",
      priority: 1,
      score: 10,
      category: "transaction",
      tags: ["tx", "rollback"],
    });
    throw Object.assign(new Error("expected rollback"), { code: "EXPECTED_ROLLBACK" });
  });
  const rollbackCount = expectData(
    await db.tasks.count({ title: "Tx rollback task" } as never),
    "transactionShowcase.rollbackCount",
  ) as number;

  const nested = await db.transaction(async (tx) => {
    const outer = await tx.tasks.insert({
      workspaceId,
      ownerId,
      title: "Tx outer task",
      description: "Outer transaction should survive the inner failure",
      status: "open",
      priority: 2,
      score: 33,
      category: "transaction",
      tags: ["tx", "outer"],
    });

    const inner = await db.transaction(async (tx2) => {
      await tx2.tasks.insert({
        workspaceId,
        ownerId,
        title: "Tx inner task",
        description: "Inner savepoint should roll back",
        status: "open",
        priority: 1,
        score: 5,
        category: "transaction",
        tags: ["tx", "inner"],
      });
      throw Object.assign(new Error("inner abort"), { code: "INNER_ABORT" });
    });

    const outerSeen = await tx.tasks.find({ id: outer.id } as never).unique();
    return {
      outer: summarizeTask(outer as unknown as Record<string, unknown>),
      outerSeen: summarizeTask(outerSeen as unknown as Record<string, unknown>),
      innerErrorCode: (inner.error as { code?: string } | null)?.code ?? null,
    };
  });
  if (nested.error) throw nested.error;

  const outerCount = expectData(
    await db.tasks.count({ title: "Tx outer task" } as never),
    "transactionShowcase.outerCount",
  ) as number;
  const innerCount = expectData(
    await db.tasks.count({ title: "Tx inner task" } as never),
    "transactionShowcase.innerCount",
  ) as number;

  return {
    commit: commit.data,
    rollback: {
      errorCode: (rollback.error as { code?: string } | null)?.code ?? null,
      visibleCount: rollbackCount,
    },
    nested: {
      ...(nested.data as Record<string, unknown>),
      outerCount,
      innerCount,
    },
  };
}, { id: "db-e2e.transaction-showcase" });

export const searchShowcase = action(async ({
  workspaceId,
}: {
  workspaceId: WorkspaceId;
}) => {
  const text = expectData(
    await db.places.search({
      text: "coffee",
      limit: 3,
      filter: { workspaceId },
    }),
    "searchShowcase.text",
  ) as Array<Record<string, unknown>>;
  const vector = expectData(
    await db.places.search({
      vector: [1, 0, 0, 0],
      k: 2,
      filter: { workspaceId },
    }),
    "searchShowcase.vector",
  ) as Array<Record<string, unknown>>;
  const near = expectData(
    await db.places.near({
      field: "loc",
      point: { lat: 37.7749, lng: -122.4194 },
      radius: 1500,
      filter: { workspaceId },
      limit: 5,
    }),
    "searchShowcase.near",
  ) as Array<Record<string, unknown>>;

  return {
    text: text.map((row) => ({ ...summarizePlace(row), _rank: row._rank })),
    vector: vector.map((row) => ({ ...summarizePlace(row), _distance: row._distance })),
    near: near.map((row) => ({ ...summarizePlace(row), _distance_m: row._distance_m })),
  };
}, { id: "db-e2e.search-showcase" });

export const securityShowcase = action(async ({ userId }: { userId: UserId }) => {
  const user = expectSome(
    expectData(await db.users.get(userId), "securityShowcase.user") as Record<string, unknown> | null,
    "securityShowcase.user",
  ) as Record<string, unknown> & {
    ssn: {
      toString(): string;
      canUnmask(opts?: { actor?: SupportActor | AdminActor | GuestActor }): Promise<boolean>;
      unmask(
        opts?: { actor?: SupportActor | AdminActor | GuestActor; reason?: string },
      ): Promise<string>;
      unmask(
        columns: readonly string[],
        opts: { actor: SupportActor | AdminActor; reason?: string },
      ): Promise<Record<string, string>>;
    };
    contactEmail: {
      toString(): string;
    };
  };

  const support = supportActor();
  const admin = adminActor();
  const guest = guestActor();

  const canSupport = await user.ssn.canUnmask({ actor: support });
  const canGuest = await user.ssn.canUnmask({ actor: guest });

  let deniedCode: string | null = null;
  try {
    await user.ssn.unmask({ actor: guest, reason: "expected denial" });
  } catch (error) {
    deniedCode = (error as { code?: string }).code ?? null;
  }

  const plainSsn = await user.ssn.unmask({
    actor: support,
    reason: "support verification",
  });
  const rowReveal = await user.ssn.unmask(["ssn", "contactEmail"], {
    actor: support,
    reason: "row reveal",
  });
  const bulk = expectData(
    await db.users.bulkUnmask(
      [{ id: userId, columns: ["ssn", "contactEmail"] }],
      { actor: admin, reason: "bulk reveal" },
    ),
    "securityShowcase.bulk",
  );
  const hinted = expectSome(
    expectData(
      await db.users
        .find(
          { id: userId } as never,
          {
            unmask: ["ssn"],
            actor: support,
            unmaskReason: "query hint",
          } as never,
        )
        .first(),
      "securityShowcase.hinted",
    ) as Record<string, unknown> | null,
    "securityShowcase.hinted",
  );

  return {
    masked: {
      ssn: String(user.ssn),
      contactEmail: String(user.contactEmail),
    },
    can: {
      support: canSupport,
      guest: canGuest,
    },
    deniedCode,
    plainSsn,
    rowReveal,
    bulk: mapBulkUnmask(bulk),
    hinted: {
      ssn: hinted.ssn,
      contactEmail: String(hinted.contactEmail),
    },
  };
}, { id: "db-e2e.security-showcase" });

export const liveTasks = stream(async function* ({
  workspaceId,
}: {
  workspaceId: WorkspaceId;
}) {
  const live = db.live(() =>
    db.tasks.find({ workspaceId }).sort({ id: 1 }),
  );

  try {
    for await (const rows of live) {
      yield mapRows(
        rows as Array<Record<string, unknown>>,
        summarizeTask,
      );
    }
  } finally {
    live.close();
  }
}, { id: "db-e2e.live-tasks" });
