/**
 * In-memory mock of `NativeMigrations` for SDK unit tests.
 *
 * Models a single migration's row set, audit row, and cancel flag.
 * Mirrors the Rust state machine just enough that the SDK loop's
 * happy path + dry-run + cancel + dead-letter assertions can run in
 * Node without a Postgres instance.
 */

import type { NativeMigration, NativeMigrations } from "../src/native.js";

export interface MockOpts {
  /** Initial table rows. `id` must be present. */
  rows: Array<Record<string, unknown> & { id: number }>;
  /**
   * If set, the mock fails advisory-lock acquisition on `start`
   * by returning `{ error: { code: "migration_already_running", ... } }`.
   */
  alreadyRunning?: boolean;
}

export interface MockState {
  /** Live table — mock applies UPDATEs in place. */
  rows: Array<Record<string, unknown> & { id: number }>;
  /** Audit row state. */
  audit: {
    auditId: number;
    cursor: number;
    processed: number;
    status: string;
    deadLetterPks: number[];
    error: string | null;
    exists: boolean;
  };
  /** True after `cancel`; flips fetch into error mode. */
  cancelled: boolean;
  /** Recorded calls. */
  calls: Array<{ method: string; args: unknown[] }>;
  /** True between start() and final commit (isDone). */
  active: boolean;
  /** True for dry-run sessions. */
  dryRun: boolean;
}

export function createMockNative(opts: MockOpts): { native: NativeMigrations; state: MockState } {
  const initialRows = opts.rows.map((r) => ({ ...r }));
  const state: MockState = {
    rows: initialRows.map((r) => ({ ...r })),
    audit: {
      auditId: 1,
      cursor: 0,
      processed: 0,
      status: "pending",
      deadLetterPks: [],
      error: null,
      exists: false,
    },
    cancelled: false,
    calls: [],
    active: false,
    dryRun: false,
  };

  function record(method: string, args: unknown[]) {
    state.calls.push({ method, args });
  }

  function envelope(payload: unknown): string {
    return JSON.stringify(payload);
  }

  function rejectWith(code: string, message: string): Promise<string> {
    // Native side resolves with `{ error: { code, message } }` for
    // structured errors. We emulate by rejecting with the JSON string —
    // toNativeError() unwraps it.
    return Promise.reject(new Error(JSON.stringify({ code, message })));
  }

  function makeMigration(): NativeMigration {
    return {
      async status() {
        record("Migration.status", []);
        const a = state.audit;
        return envelope({
          exists: a.exists,
          status: a.exists ? a.status : null,
          cursor: a.cursor,
          processed: a.processed,
          deadLetterPks: [...a.deadLetterPks],
          isDone: ["applied", "applied_with_dead_letter", "failed", "cancelled"].includes(a.status),
          error: a.error,
        });
      },
      async cancel() {
        record("Migration.cancel", []);
        state.cancelled = true;
        state.audit.status = "cancelled";
        return envelope({ ok: true });
      },
      async reset() {
        record("Migration.reset", []);
        state.audit = {
          auditId: state.audit.auditId,
          cursor: 0,
          processed: 0,
          status: "pending",
          deadLetterPks: [],
          error: null,
          exists: state.audit.exists,
        };
        return envelope({ ok: true });
      },
      async fetchBatch(cursor, batchSize) {
        record("fetchBatch", [cursor, batchSize]);
        if (state.cancelled) {
          return rejectWith("migration_cancelled", "cancelled by operator");
        }
        if (!state.active) {
          return rejectWith("no_active_migration", "start not called");
        }
        const slice = state.rows
          .filter((r) => r.id > cursor)
          .sort((a, b) => a.id - b.id)
          .slice(0, batchSize);
        return envelope({ rows: slice });
      },
      async commitBatch(updatesJson, deadLetterPksJson, nextCursor, processedTotal, isDone, terminalStatus, errorMessage) {
        record("commitBatch", [
          updatesJson,
          deadLetterPksJson,
          nextCursor,
          processedTotal,
          isDone,
          terminalStatus,
          errorMessage,
        ]);
        if (!state.active) {
          return rejectWith("no_active_migration", "start not called");
        }
        const updates: Array<{ id: number; set: Record<string, unknown> }> =
          JSON.parse(updatesJson);
        const dlp: number[] = JSON.parse(deadLetterPksJson);

        if (!state.dryRun) {
          for (const upd of updates) {
            const row = state.rows.find((r) => r.id === upd.id);
            if (row) Object.assign(row, upd.set);
          }
          state.audit.cursor = nextCursor;
          state.audit.processed = processedTotal;
          state.audit.deadLetterPks = [...dlp];
        }

        if (isDone) {
          state.active = false;
          if (state.dryRun) {
            // dry-run leaves persisted state untouched.
            state.audit.status = state.audit.status === "running" ? "pending" : state.audit.status;
          } else {
            state.audit.status = terminalStatus || "applied";
            if (errorMessage) state.audit.error = errorMessage;
          }
        }
        return envelope({ committed: !state.dryRun, done: isDone });
      },
    };
  }

  const native: NativeMigrations = {
    async start(spec) {
      const { name, collection, dryRun, reset } = spec;
      record("start", [name, collection, dryRun, reset]);
      if (opts.alreadyRunning) {
        return rejectWith("migration_already_running", "another worker has it") as unknown as Promise<NativeMigration>;
      }
      if (state.audit.status === "cancelled" && !reset) {
        return rejectWith("migration_cancelled", "previously cancelled — pass reset:true") as unknown as Promise<NativeMigration>;
      }
      if (reset) {
        state.audit = {
          auditId: state.audit.auditId,
          cursor: 0,
          processed: 0,
          status: "running",
          deadLetterPks: [],
          error: null,
          exists: true,
        };
      } else {
        state.audit.status = "running";
        state.audit.exists = true;
      }
      state.cancelled = false;
      state.active = true;
      state.dryRun = !!dryRun;
      return makeMigration();
    },

    async status(spec) {
      record("status", [spec.name, spec.collection]);
      const a = state.audit;
      return envelope({
        exists: a.exists,
        status: a.exists ? a.status : null,
        cursor: a.cursor,
        processed: a.processed,
        deadLetterPks: [...a.deadLetterPks],
        isDone: ["applied", "applied_with_dead_letter", "failed", "cancelled"].includes(a.status),
        error: a.error,
      });
    },

    async cancel(spec) {
      record("cancel", [spec.name, spec.collection]);
      if (!["pending", "running"].includes(state.audit.status)) {
        return rejectWith("migration_not_cancellable", `state '${state.audit.status}'`);
      }
      state.cancelled = true;
      state.audit.status = "cancelled";
      return envelope({ ok: true });
    },

    async reset(spec) {
      record("reset", [spec.name, spec.collection]);
      state.audit = {
        auditId: state.audit.auditId,
        cursor: 0,
        processed: 0,
        status: "pending",
        deadLetterPks: [],
        error: null,
        exists: state.audit.exists,
      };
      state.cancelled = false;
      return envelope({ ok: true });
    },
  };

  return { native, state };
}
