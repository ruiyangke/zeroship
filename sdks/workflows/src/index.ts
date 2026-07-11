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
  maxAttempts?: number;
}

export interface BackoffConfig {
  base?: string;
  max?: string;
  factor?: number;
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
  retries?: RetryConfig;
  backoff?: BackoffConfig;
  timeout?: string;
  output?: "auto" | "inline" | "ref" | "blob" | "stream" | { as: "ref" | "blob" | "stream"; contentType?: string };
  compensate?: Compensator<T>;
}

export interface StepContext {
  readonly runId: string;
  readonly workflowName: string;
  readonly ordinal: number;
  readonly name: string;
  readonly attempt: number;
  readonly idempotencyKey: string;
  readonly trigger: WorkflowTrigger<unknown>;
  readonly cause?: unknown;
}

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
  run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
  run<T>(
    name: string,
    config: StepConfig<T> & { output: "ref" | "blob" | "stream" | { as: "ref" | "blob" | "stream"; contentType?: string } },
    fn: () => T | Promise<T>,
  ): Promise<StepOutputRef>;
  run<T>(name: string, config: StepConfig<T>, fn: () => T | Promise<T>): Promise<T>;
  sideEffect<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
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
  readonly id: string;
  signal(opts: { type: string; payload?: unknown; idempotencyKey?: string }): Promise<void>;
  status(): Promise<{ state: WorkflowRunState; output?: StatusOutput<Output>; error?: unknown }>;
  pause(): Promise<void>;
  resume(): Promise<void>;
  cancel(opts?: { mode?: "abort" | "compensate" }): Promise<void>;
  restart(opts?: RestartOptions): Promise<WorkflowRun<Output>>;
  createSignalToken(opts: { types: string[]; ttl: string }): Promise<string>;
}

export class PermanentError extends Error {
  constructor(message = "permanent workflow error") {
    super(message);
    this.name = "PermanentError";
  }
}

export class StepTimeoutError extends Error {
  readonly retryable = true;

  constructor(message = "workflow step timed out") {
    super(message);
    this.name = "StepTimeoutError";
  }
}

export class WorkflowStepTimeoutError extends Error {
  readonly retryable = true;

  constructor(message = "workflow step timed out") {
    super(message);
    this.name = "WorkflowStepTimeoutError";
  }
}

export class WorkflowTimeoutError extends Error {
  readonly retryable = false;

  constructor(message = "workflow signal wait timed out") {
    super(message);
    this.name = "WorkflowTimeoutError";
  }
}

export class ChildCancelledError extends Error {
  readonly retryable = false;

  constructor(message = "child workflow was cancelled") {
    super(message);
    this.name = "ChildCancelledError";
  }
}

export class ChildTimeoutError extends Error {
  readonly retryable = false;

  constructor(message = "child workflow timed out") {
    super(message);
    this.name = "ChildTimeoutError";
  }
}

export class LimitExceededError extends Error {
  readonly retryable = false;

  constructor(message = "workflow limit exceeded") {
    super(message);
    this.name = "LimitExceededError";
  }
}

export class CompensableCarryError extends Error {
  readonly retryable = false;

  constructor(message = "cannot continue as new while compensable steps are pending") {
    super(message);
    this.name = "CompensableCarryError";
  }
}

export class RestartError extends Error {
  constructor(message = "workflow restart failed") {
    super(message);
    this.name = "RestartError";
  }
}

export class NondeterministicError extends Error {
  readonly retryable = false;

  constructor(message = "workflow replay is nondeterministic") {
    super(message);
    this.name = "NondeterministicError";
  }
}

export class StalledError extends Error {
  readonly retryable = false;

  constructor(message = "workflow run stalled") {
    super(message);
    this.name = "StalledError";
  }
}

export class NestedStepError extends Error {
  readonly retryable = false;

  constructor(message = "workflow step methods cannot be called from inside a step body") {
    super(message);
    this.name = "NestedStepError";
  }
}

export class WorkflowNestedStepError extends Error {
  readonly retryable = false;

  constructor(message = "workflow step methods cannot be called from inside a step body") {
    super(message);
    this.name = "WorkflowNestedStepError";
  }
}

export class UnsupportedError extends Error {
  readonly retryable = false;

  constructor(message = "workflow operation is not supported by this runtime") {
    super(message);
    this.name = "UnsupportedError";
  }
}

export class WorkflowUnsupportedError extends Error {
  readonly retryable = false;

  constructor(message = "workflow operation is not supported by this runtime") {
    super(message);
    this.name = "WorkflowUnsupportedError";
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
