import {
  PermanentError,
  WorkflowNestedStepError,
  WorkflowStepTimeoutError,
  WorkflowTimeoutError,
  WorkflowUnsupportedError,
  type ChildWorkflowOptions,
  type SignalEnvelope,
  type StepConfig,
  type WaitForSignalOptions,
  type Workflow,
  type WorkflowStep,
  type WorkflowTrigger,
} from "./index.js";

export const STEP_PROMISE_BRAND = Symbol.for("zeroship.workflow.stepPromise");

export type JournalStepKind = "run" | "sleep" | "wait_signal" | "child";
export type JournalStepState = "running" | "completed" | "failed";

export interface JournalStepRecord {
  ordinal: number;
  name: string;
  nameOccurrence?: number;
  kind: JournalStepKind;
  state: JournalStepState;
  output?: unknown;
  error?: { type?: string; message?: string; stack?: string; retryable?: boolean };
  wakeAt?: string;
  signalType?: string;
  consumedSignal?: SignalEnvelope<unknown> | null;
  childRunId?: string;
}

export interface JournalEnvelope {
  runId: string;
  workflowName: string;
  trigger: WorkflowTrigger<unknown>;
  steps: JournalStepRecord[];
}

export type FrontierOutcome =
  | {
      kind: "run";
      ordinal: number;
      name: string;
      nameOccurrence: number;
      state: "completed";
      output: unknown;
      config?: StepConfig<unknown>;
    }
  | {
      kind: "run";
      ordinal: number;
      name: string;
      nameOccurrence: number;
      state: "failed";
      error: { type: string; message: string; stack?: string; retryable?: boolean };
      config?: StepConfig<unknown>;
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
      options?: ChildWorkflowOptions;
    };

export class SuspendSignal extends Error {
  readonly outcome: FrontierOutcome;

  constructor(outcome: FrontierOutcome) {
    super("workflow dispatch frontier reached");
    this.name = "SuspendSignal";
    this.outcome = outcome;
  }
}

export function createJournalStep(envelope: JournalEnvelope): WorkflowStep {
  return new JournalBackedStep(envelope);
}

export function isWorkflowStepPromise(value: unknown): boolean {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    (value as Record<symbol, unknown>)[STEP_PROMISE_BRAND] === true
  );
}

export function withWorkflowPromiseGuards<T>(fn: () => T): T {
  installPromiseGuards();
  try {
    const result = fn();
    if (isThenable(result)) {
      return Promise.resolve(result).finally(uninstallPromiseGuards) as T;
    }
    uninstallPromiseGuards();
    return result;
  } catch (e) {
    uninstallPromiseGuards();
    throw e;
  }
}

class JournalBackedStep implements WorkflowStep {
  readonly #envelope: JournalEnvelope;
  readonly #stepsByOrdinal = new Map<number, JournalStepRecord>();
  readonly #nameOccurrences = new Map<string, number>();
  readonly #frontierLatch = brandStepPromise(new Promise<never>(() => {}));
  #cursor = 0;
  #frontierStarted = false;
  #insideStepCallback = false;
  #callbackSyncDepth = 0;
  #parallelIssueWindow = false;
  #parallelIssueWindowToken = 0;

  constructor(envelope: JournalEnvelope) {
    this.#envelope = envelope;
    for (const row of envelope.steps) {
      this.#stepsByOrdinal.set(row.ordinal, row);
    }
  }

  run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
  run<T>(name: string, config: StepConfig<T>, fn: () => T | Promise<T>): Promise<T>;
  run<T>(
    name: string,
    configOrFn: StepConfig<T> | (() => T | Promise<T>),
    maybeFn?: () => T | Promise<T>,
  ): Promise<T> {
    this.#assertNotNested();
    const config = typeof configOrFn === "function" ? undefined : configOrFn;
    const fn = typeof configOrFn === "function" ? configOrFn : maybeFn;
    if (typeof fn !== "function") {
      return brandStepPromise(Promise.reject(
        new WorkflowUnsupportedError("step.run requires a function body"),
      ));
    }

    const issued = this.#issue(name, "run");
    if (issued.record) {
      return this.#recordPromise<T>(issued.record);
    }
    if (this.#frontierStarted) {
      return this.#frontierLatch as Promise<T>;
    }

    this.#frontierStarted = true;
    return brandStepPromise(this.#runFrontier(
      issued,
      name,
      config as StepConfig<unknown> | undefined,
      fn as () => T | Promise<T>,
    ));
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
    if (this.#frontierStarted) return this.#frontierLatch as Promise<void>;
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
    if (this.#frontierStarted) return this.#frontierLatch as Promise<void>;
    return this.#suspendFrontier({
      kind: "sleep",
      ordinal: issued.ordinal,
      name,
      nameOccurrence: issued.nameOccurrence,
      state: "running",
      wakeAt: target.toISOString(),
    });
  }

  waitForSignal<P = unknown>(
    name: string,
    opts: WaitForSignalOptions = {},
  ): Promise<SignalEnvelope<P> | null> {
    this.#assertNotNested();
    const issued = this.#issue(name, "wait_signal");
    if (issued.record) {
      if (issued.record.state === "completed") {
        const value = issued.record.output === null
          ? null
          : issued.record.output !== undefined
            ? issued.record.output
            : issued.record.consumedSignal ?? null;
        return brandStepPromise(Promise.resolve(value as SignalEnvelope<P> | null));
      }
      return this.#recordPromise<SignalEnvelope<P> | null>(issued.record, {
        kind: "wait_signal",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        signalType: issued.record.signalType ?? opts.type ?? name,
        timeout: opts.timeout,
        maxSignalAge: opts.maxSignalAge,
        topic: opts.topic,
      });
    }
    if (this.#frontierStarted) {
      return this.#frontierLatch as Promise<SignalEnvelope<P> | null>;
    }
    return this.#suspendFrontier({
      kind: "wait_signal",
      ordinal: issued.ordinal,
      name,
      nameOccurrence: issued.nameOccurrence,
      state: "running",
      signalType: opts.type ?? name,
      timeout: opts.timeout,
      maxSignalAge: opts.maxSignalAge,
      topic: opts.topic,
    });
  }

  call<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    input: P,
    opts?: ChildWorkflowOptions,
  ): Promise<O> {
    this.#assertNotNested();
    const name = WorkflowClass.name;
    const issued = this.#issue(name, "child");
    if (issued.record) {
      return this.#recordPromise<O>(issued.record);
    }
    if (this.#frontierStarted) return this.#frontierLatch as Promise<O>;
    return this.#suspendFrontier({
      kind: "child",
      ordinal: issued.ordinal,
      name,
      nameOccurrence: issued.nameOccurrence,
      state: "running",
      workflowName: name,
      input,
      options: opts,
    });
  }

  async #runFrontier<T>(
    issued: { ordinal: number; nameOccurrence: number },
    name: string,
    config: StepConfig<unknown> | undefined,
    fn: () => T | Promise<T>,
  ): Promise<T> {
    const bodyPromise = this.#invokeStepBody(fn);
    try {
      const output = await this.#withTimeout(bodyPromise, config?.timeout);
      throw new SuspendSignal({
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "completed",
        output,
        config,
      });
    } catch (e) {
      if (e instanceof SuspendSignal) throw e;
      if (e instanceof WorkflowNestedStepError) throw e;
      throw new SuspendSignal({
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "failed",
        error: serializeError(e),
        config,
      });
    } finally {
      bodyPromise.catch(() => {});
      this.#insideStepCallback = false;
      this.#parallelIssueWindow = false;
    }
  }

  #invokeStepBody<T>(fn: () => T | Promise<T>): Promise<T> {
    this.#insideStepCallback = true;
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

  #withTimeout<T>(bodyPromise: Promise<T>, timeout: string | undefined): Promise<T> {
    const timeoutMs = parseDurationMs(timeout ?? "10m");
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    let timedOut = false;
    const timeoutPromise = new Promise<never>((_, reject) => {
      timeoutId = setTimeout(() => {
        timedOut = true;
        reject(new WorkflowStepTimeoutError(
          `workflow step timed out after ${timeout ?? "10m"}`,
        ));
      }, timeoutMs);
    });

    return Promise.race([bodyPromise, timeoutPromise]).finally(() => {
      if (timeoutId !== undefined) clearTimeout(timeoutId);
      if (timedOut) bodyPromise.catch(() => {});
    });
  }

  #suspendFrontier<T>(outcome: FrontierOutcome): Promise<T> {
    this.#frontierStarted = true;
    return brandStepPromise(Promise.reject(new SuspendSignal(outcome)));
  }

  #recordPromise<T>(record: JournalStepRecord, pendingOutcome?: FrontierOutcome): Promise<T> {
    try {
      return brandStepPromise(Promise.resolve(this.#resolveRecord<T>(record, pendingOutcome)));
    } catch (e) {
      if (e instanceof SuspendSignal) this.#frontierStarted = true;
      return brandStepPromise(Promise.reject(e));
    }
  }

  #issue(name: string, kind: JournalStepKind): {
    ordinal: number;
    nameOccurrence: number;
    record?: JournalStepRecord;
  } {
    const ordinal = this.#cursor++;
    const nameOccurrence = this.#nameOccurrences.get(name) ?? 0;
    this.#nameOccurrences.set(name, nameOccurrence + 1);

    const record = this.#stepsByOrdinal.get(ordinal);
    if (record) {
      if (
        record.name !== name ||
        record.kind !== kind ||
        (record.nameOccurrence ?? 0) !== nameOccurrence
      ) {
        throw new WorkflowUnsupportedError(
          `workflow journal mismatch at ordinal ${ordinal}: expected ${kind} ${name}#${nameOccurrence}, got ${record.kind} ${record.name}#${record.nameOccurrence ?? 0}`,
        );
      }
    }
    return { ordinal, nameOccurrence, record };
  }

  #resolveRecord<T>(record: JournalStepRecord, pendingOutcome?: FrontierOutcome): T {
    if (record.state === "completed") {
      return record.output as T;
    }
    if (record.state === "failed") {
      throw deserializeError(record.error);
    }
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
      this.#insideStepCallback &&
      (this.#callbackSyncDepth > 0 || !this.#parallelIssueWindow)
    ) {
      throw new WorkflowNestedStepError();
    }
  }
}

function brandStepPromise<T>(promise: Promise<T>): Promise<T> {
  if (!isWorkflowStepPromise(promise)) {
    Object.defineProperty(promise, STEP_PROMISE_BRAND, {
      value: true,
      configurable: false,
      enumerable: false,
      writable: false,
    });
  }
  return promise;
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    typeof (value as { then?: unknown }).then === "function"
  );
}

function serializeError(
  e: unknown,
): { type: string; message: string; stack?: string; retryable?: boolean } {
  if (e instanceof Error) {
    const retryable = (e as Error & { retryable?: unknown }).retryable;
    return {
      type: e.name || "Error",
      message: e.message,
      ...(e.stack ? { stack: e.stack } : {}),
      ...(typeof retryable === "boolean"
        ? { retryable }
        : {}),
    };
  }
  return { type: "Error", message: String(e) };
}

function deserializeError(error: JournalStepRecord["error"]): Error {
  const message = error?.message ?? "workflow step failed";
  const e = error?.type === "PermanentError"
    ? new PermanentError(message)
    : error?.type === "WorkflowStepTimeoutError" || error?.type === "StepTimeoutError"
      ? new WorkflowStepTimeoutError(message)
      : error?.type === "WorkflowTimeoutError"
        ? new WorkflowTimeoutError(message)
      : error?.type === "WorkflowNestedStepError" || error?.type === "NestedStepError"
        ? new WorkflowNestedStepError(message)
        : error?.type === "WorkflowUnsupportedError" || error?.type === "UnsupportedError"
          ? new WorkflowUnsupportedError(message)
          : new Error(message);
  e.name = error?.type ?? e.name;
  if (error?.stack) e.stack = error.stack;
  return e;
}

function parseDurationMs(raw: string): number {
  const value = raw.trim();
  const match = /^(\d+(?:\.\d+)?)\s*(ms|s|m|h)?$/i.exec(value);
  if (!match) {
    throw new WorkflowUnsupportedError(`invalid workflow step timeout: ${raw}`);
  }
  const amount = Number(match[1]);
  const unit = (match[2] ?? "ms").toLowerCase();
  const multiplier = unit === "h"
    ? 60 * 60 * 1000
    : unit === "m"
      ? 60 * 1000
      : unit === "s"
        ? 1000
        : 1;
  const ms = Math.ceil(amount * multiplier);
  if (!Number.isFinite(ms) || ms < 0) {
    throw new WorkflowUnsupportedError(`invalid workflow step timeout: ${raw}`);
  }
  return ms;
}

type PromiseCombinatorName = "race" | "allSettled" | "any";
type PromiseCombinator = (values: Iterable<unknown>) => Promise<unknown>;

let promiseGuardDepth = 0;
let originalPromiseMethods: Record<PromiseCombinatorName, PromiseCombinator> | undefined;

function installPromiseGuards(): void {
  if (promiseGuardDepth++ > 0) return;
  originalPromiseMethods = {
    race: Promise.race as PromiseCombinator,
    allSettled: Promise.allSettled as PromiseCombinator,
    any: Promise.any as PromiseCombinator,
  };
  Promise.race = guardedPromiseCombinator("race", originalPromiseMethods.race) as typeof Promise.race;
  Promise.allSettled = guardedPromiseCombinator(
    "allSettled",
    originalPromiseMethods.allSettled,
  ) as typeof Promise.allSettled;
  Promise.any = guardedPromiseCombinator("any", originalPromiseMethods.any) as typeof Promise.any;
}

function uninstallPromiseGuards(): void {
  if (promiseGuardDepth === 0) return;
  if (--promiseGuardDepth > 0) return;
  if (!originalPromiseMethods) return;
  Promise.race = originalPromiseMethods.race as typeof Promise.race;
  Promise.allSettled = originalPromiseMethods.allSettled as typeof Promise.allSettled;
  Promise.any = originalPromiseMethods.any as typeof Promise.any;
  originalPromiseMethods = undefined;
}

function guardedPromiseCombinator(
  name: PromiseCombinatorName,
  original: PromiseCombinator,
): PromiseCombinator {
  return function guarded(this: PromiseConstructor, values: Iterable<unknown>): Promise<unknown> {
    const materialized = materializeIterable(values);
    if (materialized?.some(isWorkflowStepPromise)) {
      for (const value of materialized) {
        if (isWorkflowStepPromise(value)) {
          (value as Promise<unknown>).catch(() => {});
        }
      }
      throw new WorkflowUnsupportedError(
        `Promise.${name} over workflow step promises is not supported; await steps sequentially or use Promise.all`,
      );
    }
    return Reflect.apply(original, this, [materialized ?? values]) as Promise<unknown>;
  };
}

function materializeIterable(values: Iterable<unknown>): unknown[] | undefined {
  if (
    values == null ||
    typeof (values as { [Symbol.iterator]?: unknown })[Symbol.iterator] !== "function"
  ) {
    return undefined;
  }
  return Array.from(values);
}
