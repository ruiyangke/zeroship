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
  readonly hash: string;
  readonly size: number;
  readonly contentType?: string;
  json<T = unknown>(): Promise<T>;
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
  output?: "auto" | "inline" | "ref" | { as: "ref" | "stream"; contentType?: string };
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

export interface WorkflowStep {
  run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
  run<T>(name: string, config: StepConfig<T>, fn: () => T | Promise<T>): Promise<T>;
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
}

export type Step = WorkflowStep;

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
  status(): Promise<{ state: string; output?: Output | StepOutputRef; error?: unknown }>;
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
  constructor(message = "workflow step timed out") {
    super(message);
    this.name = "StepTimeoutError";
  }
}

export class RestartError extends Error {
  constructor(message = "workflow restart failed") {
    super(message);
    this.name = "RestartError";
  }
}

export class NestedStepError extends Error {
  constructor(message = "workflow step methods cannot be called from inside a step body") {
    super(message);
    this.name = "NestedStepError";
  }
}

export class UnsupportedError extends Error {
  constructor(message = "workflow operation is not supported by this runtime") {
    super(message);
    this.name = "UnsupportedError";
  }
}
