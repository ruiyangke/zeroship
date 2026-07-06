import {
  NestedStepError,
  PermanentError,
  UnsupportedError,
  type ChildWorkflowOptions,
  type SignalEnvelope,
  type StepConfig,
  type WaitForSignalOptions,
  type Workflow,
  type WorkflowStep,
  type WorkflowTrigger,
} from "./index.js";

export type JournalStepKind = "run" | "sleep" | "wait_signal" | "child";
export type JournalStepState = "running" | "completed" | "failed";

export interface JournalStepRecord {
  ordinal: number;
  name: string;
  nameOccurrence?: number;
  kind: JournalStepKind;
  state: JournalStepState;
  output?: unknown;
  error?: { type?: string; message?: string; stack?: string };
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
      error: { type: string; message: string; stack?: string };
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

class JournalBackedStep implements WorkflowStep {
  readonly #envelope: JournalEnvelope;
  readonly #stepsByOrdinal = new Map<number, JournalStepRecord>();
  readonly #nameOccurrences = new Map<string, number>();
  #cursor = 0;
  #inStepBody = false;

  constructor(envelope: JournalEnvelope) {
    this.#envelope = envelope;
    for (const row of envelope.steps) {
      this.#stepsByOrdinal.set(row.ordinal, row);
    }
  }

  async run<T>(name: string, fn: () => T | Promise<T>): Promise<T>;
  async run<T>(name: string, config: StepConfig<T>, fn: () => T | Promise<T>): Promise<T>;
  async run<T>(
    name: string,
    configOrFn: StepConfig<T> | (() => T | Promise<T>),
    maybeFn?: () => T | Promise<T>,
  ): Promise<T> {
    this.#assertNotNested();
    const config = typeof configOrFn === "function" ? undefined : configOrFn;
    const fn = typeof configOrFn === "function" ? configOrFn : maybeFn;
    if (typeof fn !== "function") {
      throw new UnsupportedError("step.run requires a function body");
    }

    const issued = this.#issue(name, "run");
    if (issued.record) {
      return this.#resolveRecord<T>(issued.record);
    }

    try {
      this.#inStepBody = true;
      const output = await fn();
      throw new SuspendSignal({
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "completed",
        output,
        config: config as StepConfig<unknown> | undefined,
      });
    } catch (e) {
      if (e instanceof SuspendSignal) throw e;
      if (e instanceof NestedStepError) throw e;
      throw new SuspendSignal({
        kind: "run",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "failed",
        error: serializeError(e),
        config: config as StepConfig<unknown> | undefined,
      });
    } finally {
      this.#inStepBody = false;
    }
  }

  async sleep(name: string, duration: string): Promise<void> {
    this.#assertNotNested();
    const issued = this.#issue(name, "sleep");
    if (issued.record) {
      if (issued.record.state === "completed") return;
      throw new SuspendSignal({
        kind: "sleep",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        wakeAt: issued.record.wakeAt ?? duration,
      });
    }
    throw new SuspendSignal({
      kind: "sleep",
      ordinal: issued.ordinal,
      name,
      nameOccurrence: issued.nameOccurrence,
      state: "running",
      wakeAt: duration,
    });
  }

  async sleepUntil(name: string, when: Date | number): Promise<void> {
    this.#assertNotNested();
    const target = typeof when === "number" ? new Date(when) : when;
    const issued = this.#issue(name, "sleep");
    if (issued.record) {
      if (issued.record.state === "completed") return;
      throw new SuspendSignal({
        kind: "sleep",
        ordinal: issued.ordinal,
        name,
        nameOccurrence: issued.nameOccurrence,
        state: "running",
        wakeAt: issued.record.wakeAt ?? target.toISOString(),
      });
    }
    throw new SuspendSignal({
      kind: "sleep",
      ordinal: issued.ordinal,
      name,
      nameOccurrence: issued.nameOccurrence,
      state: "running",
      wakeAt: target.toISOString(),
    });
  }

  async waitForSignal<P = unknown>(
    name: string,
    opts: WaitForSignalOptions = {},
  ): Promise<SignalEnvelope<P> | null> {
    this.#assertNotNested();
    const issued = this.#issue(name, "wait_signal");
    if (issued.record) {
      if (issued.record.state === "completed") {
        if (issued.record.output === null) return null;
        if (issued.record.output !== undefined) return issued.record.output as SignalEnvelope<P>;
        return (issued.record.consumedSignal ?? null) as SignalEnvelope<P> | null;
      }
      throw new SuspendSignal({
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
    throw new SuspendSignal({
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

  async call<P, O>(
    WorkflowClass: new () => Workflow<P, O>,
    input: P,
    opts?: ChildWorkflowOptions,
  ): Promise<O> {
    this.#assertNotNested();
    const name = WorkflowClass.name;
    const issued = this.#issue(name, "child");
    if (issued.record) {
      return this.#resolveRecord<O>(issued.record);
    }
    throw new SuspendSignal({
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
        throw new UnsupportedError(
          `workflow journal mismatch at ordinal ${ordinal}: expected ${kind} ${name}#${nameOccurrence}, got ${record.kind} ${record.name}#${record.nameOccurrence ?? 0}`,
        );
      }
    }
    return { ordinal, nameOccurrence, record };
  }

  #resolveRecord<T>(record: JournalStepRecord): T {
    if (record.state === "completed") {
      return record.output as T;
    }
    if (record.state === "failed") {
      throw deserializeError(record.error);
    }
    throw new SuspendSignal({
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
    if (this.#inStepBody) {
      throw new NestedStepError();
    }
  }
}

function serializeError(e: unknown): { type: string; message: string; stack?: string } {
  if (e instanceof Error) {
    return {
      type: e.name || "Error",
      message: e.message,
      ...(e.stack ? { stack: e.stack } : {}),
    };
  }
  return { type: "Error", message: String(e) };
}

function deserializeError(error: JournalStepRecord["error"]): Error {
  const message = error?.message ?? "workflow step failed";
  const e = error?.type === "PermanentError"
    ? new PermanentError(message)
    : new Error(message);
  e.name = error?.type ?? e.name;
  if (error?.stack) e.stack = error.stack;
  return e;
}
