export interface WorkflowTrigger<Params = unknown> {
  input: Params;
  startedAt: Date;
  runId: string;
  workflowName: string;
}

export abstract class Workflow<Params = unknown, Output = unknown> {
  static concurrency?: number;
  static compensationConcurrency?: number;

  abstract run(
    trigger: WorkflowTrigger<Params>,
    step: WorkflowStep,
  ): Output | Promise<Output>;
}

export interface RetryConfig {
  /**
   * Executions of the step body, not re-executions: `1` is the default and
   * means the body runs once. Must be a positive integer, and the app's own
   * ceiling refuses anything above it rather than lowering it.
   */
  maxAttempts?: number;
}

export interface StepOutputRef {
  readonly kind: "workflow-step-output-ref";
  readonly ref: string;
  readonly hash: string;
  readonly size: number;
  readonly contentType?: string;
  json<T = unknown>(): Promise<T>;
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  stream(): ReadableStream<Uint8Array>;
}

export interface CompensationContext {
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
  readonly cause?: unknown;
}

export type Compensator<T> = (
  output: T,
  ctx: CompensationContext,
) => unknown | Promise<unknown>;

export interface StepConfig<T = unknown> {
  /**
   * How many times the body may run before its failure is final. Attempts share
   * one `ctx.idempotencyKey`, so an effect that landed before the failure was
   * reported is recognised by the external system on the next attempt.
   */
  retries?: RetryConfig;
  timeout?: string;
  output?: "auto" | "inline" | "ref" | "blob" | "stream" | { as: "ref" | "blob" | "stream"; contentType?: string };
  compensate?: Compensator<T>;
}

/**
 * Passed to every `step.run` and `step.sideEffect` body. Each field is a
 * durable journal fact, so a re-executed body receives exactly what the
 * discarded execution received.
 */
export interface StepContext {
  readonly runId: string;
  readonly workflowName: string;
  /** Position in the journal. Stable across replay; replay enforces it. */
  readonly ordinal: number;
  readonly name: string;
  /** Which issuance of `name` this is, as `RestartTarget.occurrence` counts. */
  readonly occurrence: number;
  /**
   * Stable across re-execution of this step, and distinct for every step a
   * restart re-runs. Pass it to the external system so an effect that landed
   * before its journal row was committed is not applied twice.
   */
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
}

export type StepBody<T> = (ctx: StepContext) => T | Promise<T>;

export interface SignalEnvelope<P = unknown> {
  readonly id: string;
  readonly type: string;
  readonly payload: P;
  readonly createdAt: Date;
  readonly origin?: "app" | "ingress" | "system";
  readonly delivery?: "direct" | "topic";
  readonly topic?: string;
  readonly provider?: string;
}

export interface WaitForSignalOptions {
  type?: string;
  timeout?: string;
  maxSignalAge?: string;
  topic?: string;
}

export interface ChildWorkflowOptions {
  key?: string;
  cascade?: boolean;
  timeout?: string;
}

export interface StartManyItem<P = unknown> {
  input: P;
  key?: string;
  options?: ChildWorkflowOptions;
}

export interface WorkflowStep {
  run<T>(name: string, fn: StepBody<T>): Promise<T>;
  run<T>(
    name: string,
    config: StepConfig<T> & { output: "ref" | "blob" | "stream" | { as: "ref" | "blob" | "stream"; contentType?: string } },
    fn: StepBody<T>,
  ): Promise<StepOutputRef>;
  run<T>(name: string, config: StepConfig<T>, fn: StepBody<T>): Promise<T>;
  sideEffect<T>(name: string, fn: StepBody<T>): Promise<T>;
  sleep(name: string, duration: string): Promise<void>;
  sleepUntil(name: string, when: Date | number): Promise<void>;
  /**
   * Wait for a matching signal. On timeout this resolves to `null`; there is
   * intentionally no exported SignalTimeout error class.
   */
  waitForSignal<P = unknown>(
    name: string,
    opts?: WaitForSignalOptions,
  ): Promise<SignalEnvelope<P> | null>;
  call<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    input: P,
    opts?: ChildWorkflowOptions,
  ): Promise<O>;
  startMany<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    items: readonly StartManyItem<P>[],
    opts?: ChildWorkflowOptions,
  ): Promise<O[]>;
  continueAsNew<P = unknown>(input: P): Promise<never>;
}

export type Step = WorkflowStep;
export type StatusOutput<T = unknown> = T | StepOutputRef;

export type WorkflowRunState =
  | "queued"
  | "running"
  | "sleeping"
  | "waiting"
  | "paused"
  | "stalled"
  | "compensating"
  | "completed"
  | "failed"
  | "cancelled";

export interface RestartTarget {
  name: string;
  occurrence?: number;
}

export interface RestartOptions {
  from?: RestartTarget;
  deploy?: "started" | "latest" | { pin: "started" | "latest" };
}

export interface WorkflowRun<Output = unknown> {
  /** Read a completed step through this run's app-scoped native backend. */
  readStepOutput(name: string, occurrence: number): Promise<Uint8Array>;
  readonly id: string;
  signal(opts: { type: string; payload?: unknown; idempotencyKey?: string }): Promise<void>;
  status(): Promise<{ state: WorkflowRunState; output?: StatusOutput<Output>; error?: unknown }>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  cancel(opts?: { mode?: "abort" | "compensate" }): Promise<void>;
  restart(opts?: RestartOptions): Promise<WorkflowRun<Output>>;
  createSignalToken(opts: { types: string[]; ttl: string }): Promise<string>;
}

/**
 * A workflow error is identified by its `name`, never by its constructor.
 *
 * A step failure is recorded in the journal as `{ type, message, ... }`, and the
 * host replay bridge rebuilds it on the later dispatch that rethrows it into the
 * body. The object a creator catches is therefore never the object that was
 * thrown, and no shared class could survive that round trip. `name` is the one
 * identity that does cross it, and it is the same key the journal stores as
 * `type`, so every class below matches on it.
 *
 * The brand check deliberately avoids `value instanceof Error`: the error a
 * creator catches is built by the host bridge, which is a different module from
 * this package, so the check must not depend on which `Error` constructor made
 * it.
 */
function hasWorkflowErrorName(value: unknown, name: string): boolean {
  return (
    Object.prototype.toString.call(value) === "[object Error]" &&
    (value as Error).name === name
  );
}

export class PermanentError extends Error {
  /**
   * Declared, not merely implied by the name: `retries` reads this off the
   * journal, and a business failure that cannot be cleared by running the body
   * again must not consume attempts.
   */
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is PermanentError {
    return hasWorkflowErrorName(value, "PermanentError");
  }

  constructor(message = "permanent workflow error") {
    super(message);
    this.name = "PermanentError";
  }
}

export class StepTimeoutError extends Error {
  readonly retryable = true;

  static [Symbol.hasInstance](value: unknown): value is StepTimeoutError {
    return hasWorkflowErrorName(value, "StepTimeoutError");
  }

  constructor(message = "workflow step timed out") {
    super(message);
    this.name = "StepTimeoutError";
  }
}

export class WorkflowTimeoutError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is WorkflowTimeoutError {
    return hasWorkflowErrorName(value, "WorkflowTimeoutError");
  }

  constructor(message = "workflow signal wait timed out") {
    super(message);
    this.name = "WorkflowTimeoutError";
  }
}

export class ChildCancelledError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is ChildCancelledError {
    return hasWorkflowErrorName(value, "ChildCancelledError");
  }

  constructor(message = "child workflow was cancelled") {
    super(message);
    this.name = "ChildCancelledError";
  }
}

export class ChildTimeoutError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is ChildTimeoutError {
    return hasWorkflowErrorName(value, "ChildTimeoutError");
  }

  constructor(message = "child workflow timed out") {
    super(message);
    this.name = "ChildTimeoutError";
  }
}

export class LimitExceededError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is LimitExceededError {
    return hasWorkflowErrorName(value, "LimitExceededError");
  }

  constructor(message = "workflow limit exceeded") {
    super(message);
    this.name = "LimitExceededError";
  }
}

export class CompensableCarryError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is CompensableCarryError {
    return hasWorkflowErrorName(value, "CompensableCarryError");
  }

  constructor(message = "cannot continue as new while compensable steps are pending") {
    super(message);
    this.name = "CompensableCarryError";
  }
}

export class RestartError extends Error {
  static [Symbol.hasInstance](value: unknown): value is RestartError {
    return hasWorkflowErrorName(value, "RestartError");
  }

  constructor(message = "workflow restart failed") {
    super(message);
    this.name = "RestartError";
  }
}

export class NondeterministicError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is NondeterministicError {
    return hasWorkflowErrorName(value, "NondeterministicError");
  }

  constructor(message = "workflow replay is nondeterministic") {
    super(message);
    this.name = "NondeterministicError";
  }
}

export class StalledError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is StalledError {
    return hasWorkflowErrorName(value, "StalledError");
  }

  constructor(message = "workflow run stalled") {
    super(message);
    this.name = "StalledError";
  }
}

export class NestedStepError extends Error {
  readonly retryable = false;

  static [Symbol.hasInstance](value: unknown): value is NestedStepError {
    return hasWorkflowErrorName(value, "NestedStepError");
  }

  constructor(message = "workflow step methods cannot be called from inside a step body") {
    super(message);
    this.name = "NestedStepError";
  }
}

export {
  compileSchedule,
  cronExpr,
  every,
  InvalidScheduleError,
  schedule,
} from "./schedule.js";
export type {
  CompileScheduleOptions,
  CronScheduleDescriptor,
  IntervalScheduleDescriptor,
  IntervalUnit,
  NormalizedScheduleDescriptor,
  Schedule,
  ScheduleAnchor,
  ScheduleCatchUp,
  ScheduleOverlap,
  ScheduleRegistration,
  ScheduleRegistrationOptions,
  TimeOfDay,
  TimeZone,
} from "./schedule.js";
