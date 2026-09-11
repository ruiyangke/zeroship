import { AsyncLocalStorage } from "node:async_hooks";

import {
  ChildCancelledError,
  ChildTimeoutError,
  CompensableCarryError,
  LimitExceededError,
  NondeterministicError,
  PermanentError,
  StalledError,
  WorkflowNestedStepError,
  WorkflowStepTimeoutError,
  WorkflowTimeoutError,
  WorkflowUnsupportedError,
  type ChildWorkflowOptions,
  type SignalEnvelope,
  type StartManyItem,
  type StepConfig,
  type StepOutputRef,
  type WaitForSignalOptions,
  type Workflow,
  type WorkflowStep,
  type WorkflowTrigger,
} from "./index.js";

type WorkflowDispatchContext = { mode: "body" | "step" };

export const STEP_PROMISE_BRAND = Symbol.for("zeroship.workflow.stepPromise");
const WORKFLOW_BODY_FETCH_ERROR =
  "workflow bodies may not perform I/O directly — move fetch(...) inside step.run(...) or use step.sideEffect(...)";
const WORKFLOW_BODY_TIMER_ERROR =
  "workflow bodies may not use timers directly — use step.sleep(...) instead";
const MAX_START_MANY_BATCH = 1_000;
const workflowDispatchAls = new AsyncLocalStorage<WorkflowDispatchContext>();
const workflowRealSetTimeout = globalThis.setTimeout;
const workflowRealClearTimeout = globalThis.clearTimeout;
const workflowRealFetch = globalThis.fetch;

installWorkflowIoGuards();

export type JournalStepKind = "run" | "sideEffect" | "sleep" | "wait_signal" | "child";
export type JournalStepState = "running" | "completed" | "failed";

export interface JournalStepRecord {
  ordinal: number;
  name: string;
  nameOccurrence?: number;
  kind: JournalStepKind;
  state: JournalStepState;
  output?: unknown;
  outputRef?: StepOutputRefDescriptor;
  error?: { type?: string; message?: string; stack?: string; retryable?: boolean };
  wakeAt?: string;
  signalType?: string;
  consumedSignal?: SignalEnvelope<unknown> | null;
  childRunId?: string;
  compensationState?: "pending" | "running" | "completed" | "failed";
}

export interface JournalEnvelope {
  runId: string;
  workflowName: string;
  trigger: WorkflowTrigger<unknown>;
  steps: JournalStepRecord[];
  outputRead?: WorkflowOutputReader;
}

export type WorkflowOutputReader = (name: string, occurrence: number) => Promise<Uint8Array>;

export interface StepOutputRefDescriptor {
  kind?: string;
  ref?: string;
  hash: string;
  size: number;
  contentType?: string;
}

export type FrontierOutcome =
  | {
      kind: "run" | "sideEffect";
      ordinal: number;
      name: string;
      nameOccurrence: number;
      state: "completed";
      output: unknown;
      config?: StepConfig<unknown>;
      outputMode?: string;
      outputContentType?: string;
      compensable?: boolean;
      compensationMaxAttempts?: number;
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
  readonly outcomes: readonly FrontierOutcome[];

  constructor(outcome: FrontierOutcome | readonly FrontierOutcome[]) {
    super("workflow dispatch frontier reached");
    this.name = "SuspendSignal";
    const outcomes = Array.isArray(outcome) ? outcome : [outcome];
    if (outcomes.length === 0) {
      throw new WorkflowUnsupportedError("workflow frontier batch cannot be empty");
    }
    this.outcome = outcomes[0]!;
    this.outcomes = outcomes;
  }
}

export class ContinueAsNewSignal extends Error {
  readonly input: unknown;

  constructor(input: unknown) {
    super("workflow continue-as-new requested");
    this.name = "ContinueAsNewSignal";
    this.input = input;
  }
}

export class WorkflowMicrotaskQuiescenceBarrier {
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

export function createJournalStep(
  envelope: JournalEnvelope,
  quiescence?: WorkflowMicrotaskQuiescenceBarrier,
): WorkflowStep {
  return new JournalBackedStep(envelope, quiescence);
}

export function getJournalFrontierDrainPromise(step: WorkflowStep): Promise<never> | undefined {
  return step instanceof JournalBackedStep ? step.frontierDrainPromise : undefined;
}

export function isJournalFrontierObserved(step: WorkflowStep): boolean {
  return step instanceof JournalBackedStep ? step.frontierObserved : false;
}

export function isJournalFrontierPending(step: WorkflowStep): boolean {
  return step instanceof JournalBackedStep ? step.frontierPending : false;
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

export function withWorkflowDispatchBody<T>(fn: () => T): T {
  return workflowDispatchAls.run({ mode: "body" }, fn);
}

function assertWorkflowBodyMayUseFetch(): void {
  if (workflowDispatchAls.getStore()?.mode === "body") {
    throw new NondeterministicError(WORKFLOW_BODY_FETCH_ERROR);
  }
}

function assertWorkflowBodyMayUseTimer(): void {
  if (workflowDispatchAls.getStore()?.mode === "body") {
    throw new NondeterministicError(WORKFLOW_BODY_TIMER_ERROR);
  }
}

function installWorkflowIoGuards(): void {
  const realFetch = globalThis.fetch;
  if (typeof realFetch === "function") {
    globalThis.fetch = function guardedWorkflowFetch(
      this: unknown,
      ...args: Parameters<typeof fetch>
    ): ReturnType<typeof fetch> {
      assertWorkflowBodyMayUseFetch();
      return Reflect.apply(realFetch, this, args) as ReturnType<typeof fetch>;
    };
  }

  const realSetTimeout = globalThis.setTimeout;
  if (typeof realSetTimeout === "function") {
    globalThis.setTimeout = function guardedWorkflowSetTimeout(
      this: unknown,
      ...args: Parameters<typeof setTimeout>
    ): ReturnType<typeof setTimeout> {
      assertWorkflowBodyMayUseTimer();
      return Reflect.apply(realSetTimeout, this, args) as ReturnType<typeof setTimeout>;
    };
  }

  const realSetInterval = globalThis.setInterval;
  if (typeof realSetInterval === "function") {
    globalThis.setInterval = function guardedWorkflowSetInterval(
      this: unknown,
      ...args: Parameters<typeof setInterval>
    ): ReturnType<typeof setInterval> {
      assertWorkflowBodyMayUseTimer();
      return Reflect.apply(realSetInterval, this, args) as ReturnType<typeof setInterval>;
    };
  }
}

class JournalBackedStep implements WorkflowStep {
  readonly #envelope: JournalEnvelope;
  readonly #stepsByOrdinal = new Map<number, JournalStepRecord>();
  readonly #nameOccurrences = new Map<string, number>();
  readonly #quiescence: WorkflowMicrotaskQuiescenceBarrier | undefined;
  #cursor = 0;
  #frontier: FrontierCoordinator | undefined;
  #stepWorkObserved = false;
  #activeStepCallbacks = 0;
  #callbackSyncDepth = 0;
  #parallelIssueWindow = false;
  #parallelIssueWindowToken = 0;
  readonly #outputReadMemo = new Map<string, Promise<Uint8Array>>();

  constructor(
    envelope: JournalEnvelope,
    quiescence?: WorkflowMicrotaskQuiescenceBarrier,
  ) {
    this.#envelope = envelope;
    this.#quiescence = quiescence;
    for (const row of envelope.steps) {
      this.#stepsByOrdinal.set(row.ordinal, row);
    }
  }

  get frontierDrainPromise(): Promise<never> | undefined {
    return this.#frontier?.drainPromise;
  }

  get frontierObserved(): boolean {
    return this.#stepWorkObserved || (this.#frontier?.observed ?? false);
  }

  get frontierPending(): boolean {
    return this.#frontier?.settled === false;
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
    return this.#registerFrontier(
      this.#runFrontier(
        issued,
        name,
        config as StepConfig<unknown> | undefined,
        fn as () => T | Promise<T>,
      ),
    );
  }

  sideEffect<T>(name: string, fn: () => T | Promise<T>): Promise<T> {
    this.#assertNotNested();
    if (typeof fn !== "function") {
      return brandStepPromise(Promise.reject(
        new WorkflowUnsupportedError("step.sideEffect requires a function body"),
      ));
    }

    const issued = this.#issue(name, "sideEffect");
    if (issued.record) {
      return this.#recordPromise<T>(issued.record);
    }
    return this.#registerFrontier(
      this.#sideEffectFrontier(issued, name, fn),
    );
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

  startMany<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    items: readonly StartManyItem<P>[],
    opts?: ChildWorkflowOptions,
  ): Promise<O[]> {
    this.#assertNotNested();
    const materialized = Array.from(items);
    if (materialized.length > MAX_START_MANY_BATCH) {
      return brandStepPromise(Promise.reject(new LimitExceededError(
        `startMany batch exceeds maxStartManyBatch (${materialized.length} > ${MAX_START_MANY_BATCH})`,
      )));
    }
    return brandStepPromise(Promise.all(materialized.map((item) => {
      const itemOptions = item.options ?? {};
      return this.call(WorkflowClass, item.input, {
        ...opts,
        ...itemOptions,
        ...(item.key !== undefined ? { key: item.key } : {}),
      });
    })));
  }

  continueAsNew<P>(input: P): Promise<never> {
    this.#assertNotNested();
    throw new ContinueAsNewSignal(input);
  }

  async #runFrontier<T>(
    issued: { ordinal: number; nameOccurrence: number },
    name: string,
    config: StepConfig<unknown> | undefined,
    fn: () => T | Promise<T>,
  ): Promise<FrontierOutcome> {
    const bodyPromise = this.#invokeStepBody(fn);
    try {
      const output = await this.#withTimeout(bodyPromise, config?.timeout);
      const outputConfig = workflowOutputConfig(config);
      const compensable = hasCompensator(config);
      return {
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "completed",
        output,
        config,
        ...(compensable ? { compensable: true, compensationMaxAttempts: 1 } : {}),
        ...outputConfig,
      };
    } catch (e) {
      if (e instanceof ContinueAsNewSignal) throw e;
      if (e instanceof WorkflowNestedStepError) throw e;
      return {
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "failed",
        error: serializeError(e),
        config,
      };
    } finally {
      bodyPromise.catch(() => {});
      this.#activeStepCallbacks--;
      if (this.#activeStepCallbacks === 0) {
        this.#parallelIssueWindow = false;
      }
    }
  }

  async #sideEffectFrontier<T>(
    issued: { ordinal: number; nameOccurrence: number },
    name: string,
    fn: () => T | Promise<T>,
  ): Promise<FrontierOutcome> {
    const bodyPromise = this.#invokeStepBody(fn);
    try {
      const output = await bodyPromise;
      return {
        kind: "sideEffect",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "completed",
        output,
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
      return workflowDispatchAls.run({ mode: "step" }, () => Promise.resolve(fn()));
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
      timeoutId = workflowRealSetTimeout(() => {
        timedOut = true;
        reject(new WorkflowStepTimeoutError(
          `workflow step timed out after ${timeout ?? "10m"}`,
        ));
      }, timeoutMs);
    });

    return Promise.race([bodyPromise, timeoutPromise]).finally(() => {
      if (timeoutId !== undefined) workflowRealClearTimeout(timeoutId);
      if (timedOut) bodyPromise.catch(() => {});
    });
  }

  #suspendFrontier<T>(outcome: FrontierOutcome): Promise<T> {
    return this.#registerFrontier(Promise.resolve(outcome));
  }

  #recordPromise<T>(record: JournalStepRecord, pendingOutcome?: FrontierOutcome): Promise<T> {
    try {
      return brandStepPromise(
        Promise.resolve(this.#resolveRecord<T>(record, pendingOutcome)),
        () => {
          this.#stepWorkObserved = true;
          this.#quiescence?.markProgress();
        },
      );
    } catch (e) {
      if (e instanceof SuspendSignal) {
        return this.#registerFrontier(Promise.resolve(e.outcome));
      }
      return brandStepPromise(
        Promise.reject(e),
        () => {
          this.#stepWorkObserved = true;
          this.#quiescence?.markProgress();
        },
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
    this.#quiescence?.markProgress();
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
    if (record.state === "completed") {
      if (record.outputRef) {
        return createStepOutputRef(
          record.outputRef,
          this.#envelope.outputRead,
          this.#envelope.runId,
          record.name,
          record.nameOccurrence ?? 0,
          this.#outputReadMemo,
        ) as T;
      }
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
      this.#activeStepCallbacks > 0 &&
      (this.#callbackSyncDepth > 0 || !this.#parallelIssueWindow)
    ) {
      throw new WorkflowNestedStepError();
    }
  }
}

class FrontierCoordinator {
  readonly promise: Promise<never>;
  readonly drainPromise: Promise<never>;
  readonly #quiescence: WorkflowMicrotaskQuiescenceBarrier | undefined;
  #pending = 0;
  #observed = false;
  #sealed = false;
  #settled = false;
  #fatal: unknown;
  #outcomes: FrontierOutcome[] = [];
  #reject: (reason?: unknown) => void = () => {};

  constructor(quiescence?: WorkflowMicrotaskQuiescenceBarrier) {
    this.#quiescence = quiescence;
    this.drainPromise = new Promise<never>((_, reject) => {
      this.#reject = reject;
    });
    this.promise = brandStepPromise(this.drainPromise, () => {
      this.#observed = true;
      this.#quiescence?.markProgress();
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
        this.#quiescence?.markProgress();
        this.#outcomes.push(settled);
      },
      (error) => {
        this.#quiescence?.markProgress();
        this.#fatal ??= error;
      },
    ).finally(() => {
      this.#quiescence?.markProgress();
      this.#pending--;
      this.#maybeFinish();
    });
  }

  seal(): void {
    this.#quiescence?.markProgress();
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
    : error?.type === "NondeterministicError"
      ? new NondeterministicError(message)
      : error?.type === "StalledError"
        ? new StalledError(message)
        : error?.type === "ChildCancelledError"
          ? new ChildCancelledError(message)
          : error?.type === "ChildTimeoutError"
            ? new ChildTimeoutError(message)
            : error?.type === "LimitExceededError"
            ? new LimitExceededError(message)
              : error?.type === "CompensableCarryError"
                ? new CompensableCarryError(message)
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

function workflowOutputConfig(config: StepConfig<unknown> | undefined): {
  outputMode?: string;
  outputContentType?: string;
} {
  const raw = config?.output;
  if (raw === undefined) return {};
  if (typeof raw === "string") {
    return { outputMode: raw };
  }
  return {
    outputMode: raw.as,
    ...(raw.contentType ? { outputContentType: raw.contentType } : {}),
  };
}

function hasCompensator(config: StepConfig<unknown> | undefined): boolean {
  return typeof config?.compensate === "function";
}

function createStepOutputRef(
  descriptor: StepOutputRefDescriptor,
  outputRead: WorkflowOutputReader | undefined,
  runId: string,
  name: string,
  occurrence: number,
  memo: Map<string, Promise<Uint8Array>>,
): StepOutputRef {
  const ref = descriptor.ref ?? `wfblob:sha256:${descriptor.hash}`;
  const memoKey = `${runId}:${name}:${occurrence}:${descriptor.hash}`;
  const readBytes = () => {
    let promise = memo.get(memoKey);
    if (!promise) {
      promise = readStepOutputBytes(outputRead, name, occurrence);
      memo.set(memoKey, promise);
    }
    return promise;
  };
  const readText = async () => new TextDecoder().decode(await readBytes());
  return {
    kind: "workflow-step-output-ref",
    ref,
    hash: descriptor.hash,
    size: descriptor.size,
    ...(descriptor.contentType ? { contentType: descriptor.contentType } : {}),
    async json<T = unknown>(): Promise<T> {
      return JSON.parse(await readText()) as T;
    },
    async text(): Promise<string> {
      return readText();
    },
    async arrayBuffer(): Promise<ArrayBuffer> {
      const bytes = await readBytes();
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    },
    bytes(): Promise<Uint8Array> {
      return readBytes();
    },
    stream(): ReadableStream<Uint8Array> {
      return new ReadableStream<Uint8Array>({
        async start(controller) {
          controller.enqueue(await readBytes());
          controller.close();
        },
      });
    },
  };
}

async function readStepOutputBytes(
  outputRead: WorkflowOutputReader | undefined,
  name: string,
  occurrence: number,
): Promise<Uint8Array> {
  if (typeof outputRead !== "function") {
    throw new WorkflowUnsupportedError("workflow output reader is unavailable");
  }
  return outputRead(name, occurrence);
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
