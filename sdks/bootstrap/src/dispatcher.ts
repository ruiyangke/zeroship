// Embedded RPC dispatcher (`__zsDispatch`).
//
// Compiled to `dist/dispatcher.js` and `include_str!`d by the runtime
// crate's `crates/runtime/src/core/init.rs`, spliced into the bootstrap
// module so the IIFE evaluates BEFORE `runtime-entry.js`'s top-level
// await and BEFORE the kernel resolves `default.fetch` / `default.rpc`
// off the user namespace. Dispatcher install must survive a schema-load
// failure so the worker can still surface the error via the RPC wire —
// hence: dispatcher first, schema install second.
//
// The IIFE pattern ensures idempotent install: if the bootstrap script
// is evaluated more than once (isolate refresh), the second pass keeps
// the live `__zsDispatch` rather than overwriting it. The function-
// shape `default.rpc` path documented in `docs/reference/zeroship-standard.md`
// bypasses this dispatcher entirely; the bootstrap calls the function
// directly.
//
// SOURCE OF TRUTH: this file is the canonical dispatcher. Dev mode
// (Vite plugin's `dev-bootstrap`) installs it via `dev-entry.ts` which
// imports this module for its side effect; production splices it into
// the runtime's bootstrap module via `include_str!`. Single
// implementation; no drift between dev and prod.

// Build emits this file with `export {};` to mark it as a module. The
// post-build step in `scripts/post-build.mjs` strips that line (and
// source-map comments) so the file content is pure top-level JS,
// safe to splice into the runtime's bootstrap module.
export {};

declare const globalThis: {
  __zsDispatch?: unknown;
  __zsWorkflowDispatch?: unknown;
  __zsEnterKind?: (kind: string) => number;
  __zsExitKind?: (token: number) => void;
  __zsValidateOutput?: boolean;
  // Schema-readiness promise set by `runtime-entry.ts` (production) — the
  // async DDL + mask-flush chain. The DDL is kept OFF the module-eval
  // critical path (a top-level await on it 404s the dispatch — ISS-66), so
  // the dispatcher gates the first procedure on it here. Undefined for
  // schema-less apps and the dev path (dev-entry awaits its own
  // `schemaReady` before calling through).
  __zsSchemaReady?: Promise<unknown>;
  [key: string]: unknown;
};

(function installZsDispatch(globalScope: typeof globalThis) {
  if (typeof globalScope.__zsDispatch === "function") return; // idempotent

  function isAsyncIterator(x: unknown): boolean {
    return x != null && typeof x === "object"
      && typeof (x as { [k: symbol]: unknown })[Symbol.asyncIterator] === "function"
      && typeof (x as { next?: unknown }).next === "function";
  }

  function isParseable(s: unknown): s is { parse: (input: unknown) => unknown } {
    return s != null && typeof s === "object" && typeof (s as { parse?: unknown }).parse === "function";
  }

  function zodIssues(err: unknown): unknown[] {
    const e = err as { issues?: unknown; errors?: unknown } | null;
    if (e && Array.isArray(e.issues)) return e.issues;
    if (e && Array.isArray(e.errors)) return e.errors;
    return [];
  }

  function isZodStringSchema(s: unknown): boolean {
    if (!s || typeof s !== "object") return false;
    const obj = s as {
      _def?: { typeName?: string; type?: string };
      def?: { typeName?: string; type?: string };
    };
    const def = obj._def || obj.def;
    if (!def) return false;
    if (def.typeName === "ZodString") return true;
    if (def.type === "string") return true;
    return false;
  }

  function mkErr(message: string, status: number, code: string, details?: unknown): Error {
    const e = new Error(message) as Error & { status?: number; code?: string; details?: unknown };
    e.status = status;
    e.code = code;
    if (details !== undefined) e.details = details;
    return e;
  }

  globalScope.__zsDispatch = async function dispatch(
    rpcDict: Record<string, unknown> | null | undefined,
    name: string,
    input: unknown,
    ctx: unknown,
  ): Promise<unknown> {
    if (rpcDict == null || typeof rpcDict !== "object") {
      throw mkErr("No RPC dispatch table installed", 500, "INTERNAL");
    }
    const fn = (rpcDict as Record<string, unknown>)[name] as
      | ((input: unknown, ctx: unknown) => unknown)
      | undefined;
    if (typeof fn !== "function") {
      throw mkErr("Method not found: " + name, 404, "NOT_FOUND");
    }

    // Schema-readiness gate (ISS-66). The production `runtime-entry`
    // installs the Collection wrappers synchronously but defers the async
    // DDL chain to `__zsSchemaReady` (awaiting it at module top-level
    // would 404 the dispatch). Gate the first procedure on it so handlers
    // don't race `registerModel`. A rejected chain (DDL/mask-flush
    // failure) surfaces here, through the RPC error envelope. No-op for
    // schema-less apps (undefined) and on the warm path (settled promise).
    const schemaReady = globalScope.__zsSchemaReady;
    if (schemaReady && typeof (schemaReady as { then?: unknown }).then === "function") {
      await schemaReady;
    }

    const cfg = (fn as { config?: { input?: unknown; output?: unknown; kind?: string } }).config;

    // 1. Input validation.
    let validated = input;
    if (cfg && isParseable(cfg.input)) {
      try {
        validated = cfg.input.parse(input);
      } catch (e) {
        throw mkErr("Invalid input", 400, "INVALID_ARGUMENT", { issues: zodIssues(e) });
      }
    }

    // 2. Capability frame. Falls back to no-op when natives aren't
    //    installed (legacy embeddings / tests without DbPlugin).
    const kind = (cfg && typeof cfg.kind === "string") ? cfg.kind : undefined;
    const ek = globalScope.__zsEnterKind;
    const xk = globalScope.__zsExitKind;
    const tok = (kind && typeof ek === "function") ? ek(kind) : -1;

    try {
      const result = await fn(validated, ctx);

      // 3. AsyncIterator stream framing tag. The encoder reads
      //    __zsOutputIsString to decide between AI-SDK `0:` (text) and
      //    `2:` (object) lanes.
      if (isAsyncIterator(result)) {
        if (cfg && isZodStringSchema(cfg.output)) {
          try { (result as { __zsOutputIsString?: boolean }).__zsOutputIsString = true; } catch (_e) { /* frozen */ }
        }
        return result;
      }

      // 4. Dev-only output validation. Gated on the future runtime-
      //    controlled `__zsValidateOutput` flag — opt-in, default-off.
      if (cfg && isParseable(cfg.output) && globalScope.__zsValidateOutput) {
        try {
          cfg.output.parse(result);
        } catch (e) {
          throw mkErr("Invalid handler output", 500, "INTERNAL", { issues: zodIssues(e) });
        }
      }

      return result;
    } finally {
      if (tok >= 0 && typeof xk === "function") xk(tok);
    }
  };
})(globalThis as never);

(function installZsWorkflowDispatch(globalScope: typeof globalThis) {
  if (typeof globalScope.__zsWorkflowDispatch === "function") return;

  type JournalStepKind = "run" | "sleep" | "wait_signal" | "child";
  type JournalStepState = "running" | "completed" | "failed";
  type JournalStepRecord = {
    ordinal: number;
    name: string;
    nameOccurrence?: number;
    kind: JournalStepKind;
    state: JournalStepState;
    output?: unknown;
    error?: { type?: string; message?: string; stack?: string; retryable?: boolean };
    wakeAt?: string;
    signalType?: string;
    consumedSignal?: unknown;
    childRunId?: string;
  };
  type FrontierOutcome =
    | {
        kind: "run";
        ordinal: number;
        name: string;
        nameOccurrence: number;
        state: "completed";
        output: unknown;
      }
    | {
        kind: "run";
        ordinal: number;
        name: string;
        nameOccurrence: number;
        state: "failed";
        error: { type: string; message: string; stack?: string };
      }
    | {
        kind: "sleep";
        ordinal: number;
        name: string;
        nameOccurrence: number;
        state: "running";
        wakeAt: string;
      }
    | {
        kind: "wait_signal";
        ordinal: number;
        name: string;
        nameOccurrence: number;
        state: "running";
        signalType: string;
        timeout?: string;
        maxSignalAge?: string;
        topic?: string;
      }
    | {
        kind: "child";
        ordinal: number;
        name: string;
        nameOccurrence: number;
        state: "running";
        workflowName: string;
        input: unknown;
        options?: unknown;
      };

  class SuspendSignal extends Error {
    readonly outcome: FrontierOutcome;
    readonly outcomes: readonly FrontierOutcome[];

    constructor(outcome: FrontierOutcome | readonly FrontierOutcome[]) {
      super("workflow dispatch frontier reached");
      this.name = "SuspendSignal";
      const outcomes = Array.isArray(outcome) ? outcome : [outcome];
      if (outcomes.length === 0) {
        throw mkErr("workflow frontier batch cannot be empty", 500, "WORKFLOW_DEFINITION_ERROR");
      }
      this.outcome = outcomes[0]!;
      this.outcomes = outcomes;
    }
  }

  class WorkflowTimeoutError extends Error {
    constructor(message = "workflow signal wait timed out") {
      super(message);
      this.name = "WorkflowTimeoutError";
    }
  }

  class NondeterministicError extends Error {
    readonly retryable = false;

    constructor(message = "workflow replay is nondeterministic") {
      super(message);
      this.name = "NondeterministicError";
    }
  }

  function mkErr(message: string, status: number, code: string): Error {
    const e = new Error(message) as Error & { status?: number; code?: string };
    e.status = status;
    e.code = code;
    return e;
  }

  function serializeError(e: unknown): { type: string; message: string; stack?: string; retryable?: boolean } {
    if (e instanceof Error) {
      const retryable = (e as Error & { retryable?: unknown }).retryable;
      return {
        type: e.name || "Error",
        message: e.message,
        ...(e.stack ? { stack: e.stack } : {}),
        ...(typeof retryable === "boolean" ? { retryable } : {}),
      };
    }
    return { type: "Error", message: String(e) };
  }

  function deserializeError(error: JournalStepRecord["error"]): Error {
    const e = error?.type === "WorkflowTimeoutError"
      ? new WorkflowTimeoutError(error?.message)
      : error?.type === "NondeterministicError"
        ? new NondeterministicError(error?.message)
        : new Error(error?.message ?? "workflow step failed");
    e.name = error?.type ?? e.name;
    if (error?.stack) e.stack = error.stack;
    return e;
  }

  function buildTrigger(envelope: Record<string, unknown>): Record<string, unknown> {
    const raw = envelope.trigger && typeof envelope.trigger === "object"
      ? { ...(envelope.trigger as Record<string, unknown>) }
      : {};
    raw.input = Object.prototype.hasOwnProperty.call(raw, "input") ? raw.input : envelope.input;
    raw.runId = typeof raw.runId === "string" ? raw.runId : envelope.runId;
    raw.workflowName = typeof raw.workflowName === "string" ? raw.workflowName : envelope.workflowName;
    const started = raw.startedAt;
    raw.startedAt = started instanceof Date
      ? started
      : new Date(typeof started === "string" || typeof started === "number" ? started : Date.now());
    return raw;
  }

  function normalizeJournal(envelope: Record<string, unknown>): JournalStepRecord[] {
    const source = Array.isArray(envelope.journal)
      ? envelope.journal
      : Array.isArray(envelope.steps)
        ? envelope.steps
        : [];
    return source
      .filter((row): row is Record<string, unknown> => row != null && typeof row === "object")
      .map((row) => ({
        ordinal: Number(row.ordinal),
        name: String(row.name ?? ""),
        nameOccurrence: Number(row.nameOccurrence ?? 0),
        kind: String(row.kind ?? "run") as JournalStepKind,
        state: String(row.state ?? "completed") as JournalStepState,
        output: row.output,
        error: row.error as JournalStepRecord["error"],
        wakeAt: typeof row.wakeAt === "string" ? row.wakeAt : undefined,
        signalType: typeof row.signalType === "string" ? row.signalType : undefined,
        consumedSignal: row.consumedSignal,
        childRunId: typeof row.childRunId === "string" ? row.childRunId : undefined,
      }));
  }

  class DispatchMicrotaskQuiescenceBarrier {
    #version = 0;
    #stopped = false;

    markProgress(): void {
      this.#version++;
    }

    stop(): void {
      this.#stopped = true;
      this.markProgress();
    }

    waitUntilBlocked(isLegalPending: () => boolean): Promise<never> {
      return new Promise<never>((_, reject) => {
        let lastVersion = this.#version;
        let stableProbes = 0;
        const probe = () => {
          if (this.#stopped) return;
          if (isLegalPending()) {
            this.stop();
            return;
          }
          if (this.#version !== lastVersion) {
            lastVersion = this.#version;
            stableProbes = 0;
            queueMicrotask(probe);
            return;
          }
          stableProbes++;
          if (stableProbes >= 3) {
            this.#stopped = true;
            reject(new NondeterministicError(
              "workflow body awaited non-step work outside the microtask replay boundary",
            ));
            return;
          }
          queueMicrotask(probe);
        };
        queueMicrotask(probe);
      });
    }
  }

  class JournalBackedStep {
    readonly #stepsByOrdinal = new Map<number, JournalStepRecord>();
    readonly #nameOccurrences = new Map<string, number>();
    readonly #quiescence: DispatchMicrotaskQuiescenceBarrier;
    #cursor = 0;
    #frontier: FrontierCoordinator | undefined;
    #activeStepCallbacks = 0;
    #callbackSyncDepth = 0;
    #parallelIssueWindow = false;
    #parallelIssueWindowToken = 0;

    constructor(steps: JournalStepRecord[], quiescence: DispatchMicrotaskQuiescenceBarrier) {
      this.#quiescence = quiescence;
      for (const row of steps) this.#stepsByOrdinal.set(row.ordinal, row);
    }

    get frontierDrainPromise(): Promise<never> | undefined {
      return this.#frontier?.drainPromise;
    }

    get frontierObserved(): boolean {
      return this.#frontier?.observed ?? false;
    }

    get frontierPending(): boolean {
      return this.#frontier?.settled === false;
    }

    run<T>(
      name: string,
      configOrFn: unknown,
      maybeFn?: () => T | Promise<T>,
    ): Promise<T> {
      this.#assertNotNested();
      const fn = typeof configOrFn === "function" ? configOrFn : maybeFn;
      if (typeof fn !== "function") {
        return Promise.reject(mkErr("step.run requires a function body", 500, "WORKFLOW_DEFINITION_ERROR"));
      }

      const issued = this.#issue(name, "run");
      if (issued.record) return this.#recordPromise<T>(issued.record);
      return this.#registerFrontier(this.#runFrontier(issued, name, fn as () => T | Promise<T>));
    }

    sleep(name: string, duration: string): Promise<void> {
      this.#assertNotNested();
      const issued = this.#issue(name, "sleep");
      if (issued.record) {
        if (issued.record.state === "completed") return brandStepPromise(Promise.resolve());
        return this.#recordPromise<void>(issued.record, {
          kind: "sleep",
          ordinal: issued.ordinal,
          name,
          nameOccurrence: issued.nameOccurrence,
          state: "running",
          wakeAt: issued.record.wakeAt ?? duration,
        });
      }
      return this.#suspendFrontier({
        kind: "sleep",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        wakeAt: duration,
      });
    }

    sleepUntil(name: string, when: Date | number): Promise<void> {
      this.#assertNotNested();
      const target = typeof when === "number" ? new Date(when) : when;
      const issued = this.#issue(name, "sleep");
      if (issued.record) {
        if (issued.record.state === "completed") return brandStepPromise(Promise.resolve());
        return this.#recordPromise<void>(issued.record, {
          kind: "sleep",
          ordinal: issued.ordinal,
          name,
          nameOccurrence: issued.nameOccurrence,
          state: "running",
          wakeAt: issued.record.wakeAt ?? target.toISOString(),
        });
      }
      return this.#suspendFrontier({
        kind: "sleep",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        wakeAt: target.toISOString(),
      });
    }

    waitForSignal(name: string, opts: Record<string, unknown> = {}): Promise<unknown> {
      this.#assertNotNested();
      const issued = this.#issue(name, "wait_signal");
      if (issued.record) {
        if (issued.record.state === "completed") {
          if (issued.record.output !== undefined) return brandStepPromise(Promise.resolve(issued.record.output));
          return brandStepPromise(Promise.resolve(issued.record.consumedSignal ?? null));
        }
        if (issued.record.state === "failed") return this.#recordPromise<unknown>(issued.record);
        return this.#recordPromise<unknown>(issued.record, {
          kind: "wait_signal",
          ordinal: issued.ordinal,
          name,
          nameOccurrence: issued.nameOccurrence,
          state: "running",
          signalType: issued.record.signalType ?? String(opts.type ?? name),
          timeout: typeof opts.timeout === "string" ? opts.timeout : undefined,
          maxSignalAge: typeof opts.maxSignalAge === "string" ? opts.maxSignalAge : undefined,
          topic: typeof opts.topic === "string" ? opts.topic : undefined,
        });
      }
      return this.#suspendFrontier({
        kind: "wait_signal",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        signalType: String(opts.type ?? name),
        timeout: typeof opts.timeout === "string" ? opts.timeout : undefined,
        maxSignalAge: typeof opts.maxSignalAge === "string" ? opts.maxSignalAge : undefined,
        topic: typeof opts.topic === "string" ? opts.topic : undefined,
      });
    }

    call(WorkflowClass: { new(): unknown; name?: string }, input: unknown, options?: unknown): Promise<unknown> {
      this.#assertNotNested();
      const name = WorkflowClass.name ?? "Workflow";
      const issued = this.#issue(name, "child");
      if (issued.record) return this.#recordPromise<unknown>(issued.record);
      return this.#suspendFrontier({
        kind: "child",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        workflowName: name,
        input,
        options,
      });
    }

    async #runFrontier<T>(
      issued: { ordinal: number; nameOccurrence: number },
      name: string,
      fn: () => T | Promise<T>,
    ): Promise<FrontierOutcome> {
      const bodyPromise = this.#invokeStepBody(fn);
      try {
        const output = await bodyPromise;
        return {
          kind: "run",
          ordinal: issued.ordinal,
          name,
          nameOccurrence: issued.nameOccurrence,
          state: "completed",
          output,
        };
      } catch (e) {
        if (e instanceof SuspendSignal) throw e;
        return {
          kind: "run",
          ordinal: issued.ordinal,
          name,
          nameOccurrence: issued.nameOccurrence,
          state: "failed",
          error: serializeError(e),
        };
      } finally {
        bodyPromise.catch(() => {});
        this.#activeStepCallbacks--;
        if (this.#activeStepCallbacks === 0) {
          this.#parallelIssueWindow = false;
        }
      }
    }

    #invokeStepBody<T>(fn: () => T | Promise<T>): Promise<T> {
      this.#activeStepCallbacks++;
      this.#parallelIssueWindow = true;
      const issueWindowToken = ++this.#parallelIssueWindowToken;
      queueMicrotask(() => {
        if (this.#parallelIssueWindowToken === issueWindowToken) {
          this.#parallelIssueWindow = false;
        }
      });

      this.#callbackSyncDepth++;
      try {
        return Promise.resolve(fn());
      } catch (e) {
        return Promise.reject(e);
      } finally {
        this.#callbackSyncDepth--;
      }
    }

    #suspendFrontier<T>(outcome: FrontierOutcome): Promise<T> {
      return this.#registerFrontier(Promise.resolve(outcome));
    }

    #recordPromise<T>(record: JournalStepRecord, pendingOutcome?: FrontierOutcome): Promise<T> {
      try {
        return brandStepPromise(
          Promise.resolve(this.#resolveRecord<T>(record, pendingOutcome)),
          () => this.#quiescence.markProgress(),
        );
      } catch (e) {
        if (e instanceof SuspendSignal) {
          return this.#registerFrontier(Promise.resolve(e.outcome));
        }
        return brandStepPromise(
          Promise.reject(e),
          () => this.#quiescence.markProgress(),
        );
      }
    }

    #registerFrontier<T>(outcome: Promise<FrontierOutcome>): Promise<T> {
      const frontier = this.#frontier ??= new FrontierCoordinator(this.#quiescence);
      if (!frontier.sealed) {
        frontier.add(outcome);
      }
      frontier.drainPromise.catch(() => {});
      suppressUnhandledRejection(frontier.promise);
      return frontier.promise as Promise<T>;
    }

    #issue(name: string, kind: JournalStepKind): {
      ordinal: number;
      nameOccurrence: number;
      record?: JournalStepRecord;
    } {
      const ordinal = this.#cursor++;
      this.#quiescence.markProgress();
      const nameOccurrence = this.#nameOccurrences.get(name) ?? 0;
      this.#nameOccurrences.set(name, nameOccurrence + 1);
      const record = this.#stepsByOrdinal.get(ordinal);
      if (record) {
        if (
          record.name !== name ||
          record.kind !== kind ||
          (record.nameOccurrence ?? 0) !== nameOccurrence
        ) {
          throw new NondeterministicError(
            `workflow journal mismatch at ordinal ${ordinal}: expected ${kind} ${name}#${nameOccurrence}, got ${record.kind} ${record.name}#${record.nameOccurrence ?? 0}`,
          );
        }
      }
      return { ordinal, nameOccurrence, record };
    }

    #resolveRecord<T>(record: JournalStepRecord, pendingOutcome?: FrontierOutcome): T {
      if (record.state === "completed") return record.output as T;
      if (record.state === "failed") throw deserializeError(record.error);
      throw new SuspendSignal(pendingOutcome ?? {
        kind: record.kind === "child" ? "child" : record.kind,
        ordinal: record.ordinal,
        name: record.name,
        nameOccurrence: record.nameOccurrence ?? 0,
        state: "running",
        ...(record.kind === "sleep"
          ? { wakeAt: record.wakeAt ?? "" }
          : record.kind === "wait_signal"
            ? { signalType: record.signalType ?? record.name }
            : record.kind === "child"
              ? { workflowName: record.name, input: undefined }
              : { output: record.output }),
      } as FrontierOutcome);
    }

    #assertNotNested(): void {
      if (
        this.#activeStepCallbacks > 0 &&
        (this.#callbackSyncDepth > 0 || !this.#parallelIssueWindow)
      ) {
        throw mkErr("workflow step methods cannot be called from inside a step body", 500, "WORKFLOW_DEFINITION_ERROR");
      }
    }
  }

  class FrontierCoordinator {
    readonly promise: Promise<never>;
    readonly drainPromise: Promise<never>;
    readonly #quiescence: DispatchMicrotaskQuiescenceBarrier;
    #pending = 0;
    #observed = false;
    #sealed = false;
    #settled = false;
    #fatal: unknown;
    #outcomes: FrontierOutcome[] = [];
    #reject: (reason?: unknown) => void = () => {};

    constructor(quiescence: DispatchMicrotaskQuiescenceBarrier) {
      this.#quiescence = quiescence;
      this.drainPromise = new Promise<never>((_, reject) => {
        this.#reject = reject;
      });
      this.promise = brandStepPromise(this.drainPromise, () => {
        this.#observed = true;
        this.#quiescence.markProgress();
      });
      queueMicrotask(() => {
        queueMicrotask(() => this.seal());
      });
    }

    get sealed(): boolean {
      return this.#sealed;
    }

    get observed(): boolean {
      return this.#observed;
    }

    get settled(): boolean {
      return this.#settled;
    }

    add(outcome: Promise<FrontierOutcome>): void {
      if (this.#sealed) return;
      this.#pending++;
      outcome.then(
        (settled) => {
          this.#quiescence.markProgress();
          this.#outcomes.push(settled);
        },
        (error) => {
          this.#quiescence.markProgress();
          this.#fatal ??= error;
        },
      ).finally(() => {
        this.#quiescence.markProgress();
        this.#pending--;
        this.#maybeFinish();
      });
    }

    seal(): void {
      this.#quiescence.markProgress();
      if (!this.#observed) {
        this.#fatal ??= new NondeterministicError(
          "workflow body awaited non-step work while a frontier was pending",
        );
      }
      this.#sealed = true;
      this.#maybeFinish();
    }

    #maybeFinish(): void {
      if (this.#settled || !this.#sealed || this.#pending > 0) return;
      this.#settled = true;
      if (this.#fatal !== undefined) {
        this.#reject(this.#fatal);
        return;
      }
      this.#outcomes.sort((a, b) => a.ordinal - b.ordinal);
      this.#reject(new SuspendSignal(this.#outcomes));
    }
  }

  const STEP_PROMISE_BRAND = Symbol.for("zeroship.workflow.stepPromise");

  function isWorkflowStepPromise(value: unknown): boolean {
    return (
      (typeof value === "object" || typeof value === "function") &&
      value !== null &&
      (value as Record<symbol, unknown>)[STEP_PROMISE_BRAND] === true
    );
  }

  class WorkflowStepPromise<T> extends Promise<T> {
    declare readonly [STEP_PROMISE_BRAND]: true;
    #observed = false;
    readonly #onObserve: (() => void) | undefined;

    static get [Symbol.species](): PromiseConstructor {
      return Promise;
    }

    constructor(
      executor: (
        resolve: (value: T | PromiseLike<T>) => void,
        reject: (reason?: unknown) => void,
      ) => void,
      onObserve?: () => void,
    ) {
      super(executor);
      this.#onObserve = onObserve;
      Object.defineProperty(this, STEP_PROMISE_BRAND, {
        value: true,
        configurable: false,
        enumerable: false,
        writable: false,
      });
    }

    then<TResult1 = T, TResult2 = never>(
      onfulfilled?: ((value: T) => TResult1 | PromiseLike<TResult1>) | null,
      onrejected?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
    ): Promise<TResult1 | TResult2> {
      this.#observe();
      return super.then(onfulfilled, onrejected);
    }

    catch<TResult = never>(
      onrejected?: ((reason: unknown) => TResult | PromiseLike<TResult>) | null,
    ): Promise<T | TResult> {
      this.#observe();
      return super.catch(onrejected);
    }

    finally(onfinally?: (() => void) | null): Promise<T> {
      this.#observe();
      return super.finally(onfinally);
    }

    #observe(): void {
      if (this.#observed) return;
      this.#observed = true;
      this.#onObserve?.();
    }

    suppressUnhandledRejection(): void {
      super.then(undefined, () => {});
    }
  }

  function brandStepPromise<T>(promise: Promise<T>, onObserve?: () => void): Promise<T> {
    if (isWorkflowStepPromise(promise)) return promise;
    return new WorkflowStepPromise<T>((resolve, reject) => {
      promise.then(resolve, reject);
    }, onObserve);
  }

  function suppressUnhandledRejection<T>(promise: Promise<T>): void {
    if (promise instanceof WorkflowStepPromise) {
      promise.suppressUnhandledRejection();
      return;
    }
    promise.catch(() => {});
  }

  function resolveWorkflow(userNamespace: unknown, workflowName: string): { new(): { run?: unknown } } {
    const mod = (userNamespace ?? {}) as Record<string, unknown>;
    const def = mod.default && typeof mod.default === "object"
      ? mod.default as Record<string, unknown>
      : {};
    const candidates = [
      mod[workflowName],
      (def.workflows && typeof def.workflows === "object"
        ? (def.workflows as Record<string, unknown>)[workflowName]
        : undefined),
      def[workflowName],
      typeof mod.default === "function" ? mod.default : undefined,
    ];
    const found = candidates.find((candidate) => typeof candidate === "function");
    if (!found) {
      throw mkErr(`Workflow not found: ${workflowName}`, 404, "WORKFLOW_NOT_FOUND");
    }
    return found as { new(): { run?: unknown } };
  }

  function resultFromFrontier(envelope: Record<string, unknown>, outcome: FrontierOutcome): Record<string, unknown> {
    const base = {
      runId: envelope.runId,
      nonce: envelope.nonce,
      workflowName: envelope.workflowName,
      ordinal: outcome.ordinal,
      name: outcome.name,
      nameOccurrence: outcome.nameOccurrence,
    };
    if (outcome.kind === "run" && outcome.state === "completed") {
      return { ...base, kind: "StepCompleted", output: outcome.output };
    }
    if (outcome.kind === "run" && outcome.state === "failed") {
      return { ...base, kind: "RunFailed", error: outcome.error };
    }
    if (outcome.kind === "sleep") {
      return { ...base, kind: "Sleep", wakeAt: outcome.wakeAt };
    }
    if (outcome.kind === "wait_signal") {
      return {
        ...base,
        kind: "Wait",
        signalType: outcome.signalType,
        timeout: outcome.timeout,
        maxSignalAge: outcome.maxSignalAge,
        topic: outcome.topic,
      };
    }
    return {
      ...base,
      kind: "Wait",
      childWorkflowName: outcome.workflowName,
      input: outcome.input,
      options: outcome.options,
    };
  }

  function resultBatch(
    envelope: Record<string, unknown>,
    outcomes: readonly FrontierOutcome[],
  ): Record<string, unknown> {
    const mapped = outcomes.map((outcome) => resultFromFrontier(envelope, outcome));
    const base = {
      runId: envelope.runId,
      dispatchNonce: envelope.nonce,
      workflowName: envelope.workflowName,
      outcomes: mapped,
    };
    return mapped.length === 1 ? { ...mapped[0], ...base } : base;
  }

  function terminalBatch(
    envelope: Record<string, unknown>,
    outcome: Record<string, unknown>,
  ): Record<string, unknown> {
    return {
      ...outcome,
      runId: envelope.runId,
      dispatchNonce: envelope.nonce,
      workflowName: envelope.workflowName,
      outcomes: [outcome],
    };
  }

  globalScope.__zsWorkflowDispatch = async function workflowDispatch(
    userNamespace: unknown,
    envelope: unknown,
    _ctx?: unknown,
  ): Promise<Record<string, unknown>> {
    if (envelope == null || typeof envelope !== "object") {
      throw mkErr("workflow dispatch envelope must be an object", 400, "INVALID_ARGUMENT");
    }
    const env = envelope as Record<string, unknown>;
    const workflowName = typeof env.workflowName === "string" ? env.workflowName : "";
    if (!workflowName) throw mkErr("workflowName is required", 400, "INVALID_ARGUMENT");

    try {
      const WorkflowClass = resolveWorkflow(userNamespace, workflowName);
      const workflow = new WorkflowClass();
      if (typeof workflow.run !== "function") {
        throw mkErr(`Workflow ${workflowName} has no run(trigger, step) method`, 500, "WORKFLOW_DEFINITION_ERROR");
      }
      const quiescence = new DispatchMicrotaskQuiescenceBarrier();
      const step = new JournalBackedStep(normalizeJournal(env), quiescence);
      const blockedByNonStepWork = quiescence.waitUntilBlocked(() => step.frontierObserved);
      let outputPromise: Promise<unknown>;
      try {
        outputPromise = Promise.resolve(workflow.run(buildTrigger(env), step));
      } catch (error) {
        quiescence.stop();
        blockedByNonStepWork.catch(() => {});
        step.frontierDrainPromise?.catch(() => {});
        throw error;
      }
      outputPromise.then(
        () => quiescence.stop(),
        () => quiescence.stop(),
      );
      outputPromise.catch(() => {});
      const frontierDrainPromise = step.frontierDrainPromise;
      if (frontierDrainPromise) {
        await Promise.race([
          frontierDrainPromise,
          outputPromise.then(
            () => {
              throw new NondeterministicError("workflow completed while a frontier was pending");
            },
            (error) => {
              throw error;
            },
          ),
          blockedByNonStepWork,
        ]);
      }
      const output = await Promise.race([outputPromise, blockedByNonStepWork]);
      if (step.frontierPending) {
        throw new NondeterministicError("workflow completed while a frontier was pending");
      }
      return terminalBatch(env, {
        kind: "RunCompleted",
        runId: env.runId,
        nonce: env.nonce,
        workflowName,
        output,
      });
    } catch (e) {
      if (e instanceof SuspendSignal) {
        return resultBatch(env, e.outcomes);
      }
      return terminalBatch(env, {
        kind: "RunFailed",
        runId: env.runId,
        nonce: env.nonce,
        workflowName,
        error: serializeError(e),
      });
    }
  };
})(globalThis as never);
