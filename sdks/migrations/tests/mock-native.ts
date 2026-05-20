/**
 * In-memory mock of `NativeMigrations` for SDK unit tests.
 *
 * Models a single migration's row set, audit row, and cancel flag.
 * Mirrors the Rust state machine just enough that the SDK loop's
 * happy path + dry-run + cancel + dead-letter assertions can run in
 * Node without a Postgres instance.
 */

import type { NativeMigration, NativeMigrations, NativeStatus } from "../src/native.js";

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

  function rejectWith(code: string, message: string): Promise<never> {
    // Native side now rejects with a real Error that carries `.code`
    // (and optionally `.hint`) as properties — see `OpError::coded` in
    // Rust. The mock mirrors that shape so tests exercise the same
    // SDK path as production.
    const err = new Error(message) as Error & { code: string };
    err.code = code;
    return Promise.reject(err);
  }

  function statusSnapshot(): NativeStatus {
    const a = state.audit;
    return {
      exists: a.exists,
      status: a.exists ? a.status : null,
      cursor: a.cursor,
      processed: a.processed,
      deadLetterPks: [...a.deadLetterPks],
      isDone: ["applied", "applied_with_dead_letter", "failed", "cancelled"].includes(a.status),
      error: a.error,
    };
  }

  function makeMigration(): NativeMigration {
    return {
      async status() {
        record("Migration.status", []);
        return statusSnapshot();
      },
      async cancel() {
        record("Migration.cancel", []);
        state.cancelled = true;
        state.audit.status = "cancelled";
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
        return slice;
      },
      async commitBatch(spec) {
        record("commitBatch", [spec]);
        if (!state.active) {
          await rejectWith("no_active_migration", "start not called");
        }
        const { updates, deadLetterPks, nextCursor, processedTotal, isDone, terminalStatus, errorMessage } = spec;

        if (!state.dryRun) {
          for (const upd of updates) {
            const row = state.rows.find((r) => r.id === upd.id);
            if (row) Object.assign(row, upd.set);
          }
          state.audit.cursor = nextCursor;
          state.audit.processed = processedTotal;
          state.audit.deadLetterPks = [...deadLetterPks];
        }

        if (isDone) {
          state.active = false;
          if (state.dryRun) {
            // dry-run leaves persisted state untouched.
            state.audit.status = state.audit.status === "running" ? "pending" : state.audit.status;
          } else {
            state.audit.status = terminalStatus ?? "applied";
            if (errorMessage) state.audit.error = errorMessage;
          }
        }
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
      return statusSnapshot();
    },

    async cancel(spec) {
      record("cancel", [spec.name, spec.collection]);
      if (!["pending", "running"].includes(state.audit.status)) {
        await rejectWith("migration_not_cancellable", `state '${state.audit.status}'`);
      }
      state.cancelled = true;
      state.audit.status = "cancelled";
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
    },
  };

  return { native, state };
}
