// Host-only workflow replay bridge, registered as `zeroship:workflows/dispatch`
// by `WorkflowBinding`. Creator modules cannot import it; native startup calls
// `installBodyGuards` and every replay calls `dispatch`.
import { AsyncLocalStorage } from "node:async_hooks";

const zsWorkflowDispatchAls = new AsyncLocalStorage();
const ZS_WORKFLOW_BODY_FETCH_ERROR =
    "workflow bodies may not perform I/O directly — move fetch(...) inside step.run(...) or use step.sideEffect(...)";
const ZS_WORKFLOW_BODY_TIMER_ERROR =
    "workflow bodies may not use timers directly — use step.sleep(...) instead";
class ZsNondeterministicError extends Error {
    constructor(message = "workflow replay is nondeterministic") {
        super(message);
        this.name = "NondeterministicError";
    }
}

function zsAssertWorkflowBodyMayUseFetch() {
    if (zsWorkflowDispatchAls.getStore()?.mode === "body") {
        throw new ZsNondeterministicError(ZS_WORKFLOW_BODY_FETCH_ERROR);
    }
}

function zsAssertWorkflowBodyMayUseTimer() {
    if (zsWorkflowDispatchAls.getStore()?.mode === "body") {
        throw new ZsNondeterministicError(ZS_WORKFLOW_BODY_TIMER_ERROR);
    }
}

let zsBodyGuardsInstalled = false;

// Wrap the ambient I/O globals before creator modules evaluate, so a creator
// that captures `fetch` or `setTimeout` at module scope still holds a guarded
// binding. Native startup calls this once per isolate.
export function installBodyGuards() {
    if (zsBodyGuardsInstalled) return;
    zsBodyGuardsInstalled = true;
    const realFetch = globalThis.fetch;
    if (typeof realFetch === "function") {
        globalThis.fetch = function guardedWorkflowFetch(...args) {
            zsAssertWorkflowBodyMayUseFetch();
            return Reflect.apply(realFetch, this, args);
        };
    }

    const realSetTimeout = globalThis.setTimeout;
    if (typeof realSetTimeout === "function") {
        globalThis.setTimeout = function guardedWorkflowSetTimeout(...args) {
            zsAssertWorkflowBodyMayUseTimer();
            return Reflect.apply(realSetTimeout, this, args);
        };
    }

    const realSetInterval = globalThis.setInterval;
    if (typeof realSetInterval === "function") {
        globalThis.setInterval = function guardedWorkflowSetInterval(...args) {
            zsAssertWorkflowBodyMayUseTimer();
            return Reflect.apply(realSetInterval, this, args);
        };
    }
}

class ZsWorkflowSuspendSignal extends Error {
    constructor(outcome) {
        super("workflow dispatch frontier reached");
        this.name = "SuspendSignal";
        const outcomes = Array.isArray(outcome) ? outcome : [outcome];
        if (outcomes.length === 0) {
            throw wfErr("workflow frontier batch cannot be empty", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        this.outcome = outcomes[0];
        this.outcomes = outcomes;
    }
}

class ZsWorkflowContinueAsNewSignal extends Error {
    constructor(input) {
        super("workflow continue-as-new requested");
        this.name = "ContinueAsNewSignal";
        this.input = input;
    }
}

class ZsWorkflowTimeoutError extends Error {
    constructor(message = "workflow signal wait timed out") {
        super(message);
        this.name = "WorkflowTimeoutError";
    }
}

class ZsChildCancelledError extends Error {
    constructor(message = "child workflow was cancelled") {
        super(message);
        this.name = "ChildCancelledError";
    }
}

class ZsChildTimeoutError extends Error {
    constructor(message = "child workflow timed out") {
        super(message);
        this.name = "ChildTimeoutError";
    }
}

class ZsLimitExceededError extends Error {
    constructor(message = "workflow limit exceeded") {
        super(message);
        this.name = "LimitExceededError";
    }
}

class ZsWorkflowCompensationReplayReady extends Error {
    constructor() {
        super("workflow compensation registry is ready");
        this.name = "CompensationReplayReady";
    }
}

const ZS_MAX_START_MANY_BATCH = 1000;

function wfErr(message, status, code) {
    const e = new Error(message);
    e.status = status;
    e.code = code;
    return e;
}

function wfSerializeError(e) {
    if (e instanceof Error) {
        const out = { type: e.name || "Error", message: e.message };
        if (e.stack) out.stack = e.stack;
        return out;
    }
    return { type: "Error", message: String(e) };
}

function wfDeserializeError(error) {
    const e = error && error.type === "WorkflowTimeoutError"
        ? new ZsWorkflowTimeoutError(error.message)
        : error && error.type === "NondeterministicError"
            ? new ZsNondeterministicError(error.message)
            : error && error.type === "ChildCancelledError"
                ? new ZsChildCancelledError(error.message)
                : error && error.type === "ChildTimeoutError"
                    ? new ZsChildTimeoutError(error.message)
                    : error && error.type === "LimitExceededError"
                        ? new ZsLimitExceededError(error.message)
                        : new Error((error && error.message) || "workflow step failed");
    e.name = (error && error.type) || e.name;
    if (error && error.stack) e.stack = error.stack;
    return e;
}

function wfTrigger(envelope) {
    const raw = envelope.trigger && typeof envelope.trigger === "object" ? { ...envelope.trigger } : {};
    if (!Object.prototype.hasOwnProperty.call(raw, "input")) raw.input = envelope.input;
    if (typeof raw.runId !== "string") raw.runId = envelope.runId;
    if (typeof raw.workflowName !== "string") raw.workflowName = envelope.workflowName;
    const started = raw.startedAt;
    raw.startedAt = started instanceof Date
        ? started
        : new Date(typeof started === "string" || typeof started === "number" ? started : Date.now());
    return raw;
}

function wfJournal(envelope) {
    const source = Array.isArray(envelope.journal)
        ? envelope.journal
        : Array.isArray(envelope.steps) ? envelope.steps : [];
    return source
        .filter((row) => row && typeof row === "object")
        .map((row) => ({
            ordinal: Number(row.ordinal),
            name: String(row.name ?? ""),
            nameOccurrence: Number(row.nameOccurrence ?? 0),
            kind: String(row.kind ?? "run"),
            state: String(row.state ?? "completed"),
            output: row.output,
            outputRef: wfNormalizeOutputRef(row.outputRef),
            error: row.error,
            wakeAt: typeof row.wakeAt === "string" ? row.wakeAt : undefined,
            signalType: typeof row.signalType === "string" ? row.signalType : undefined,
            consumedSignal: row.consumedSignal,
            childRunId: typeof row.childRunId === "string" ? row.childRunId : undefined,
            compensationState: typeof row.compensationState === "string" ? row.compensationState : undefined,
        }));
}

function wfNormalizeOutputRef(value) {
    if (!value || typeof value !== "object") return undefined;
    const hash = typeof value.hash === "string" ? value.hash : "";
    const size = typeof value.size === "number" ? value.size : Number(value.size);
    if (!hash || !Number.isFinite(size)) return undefined;
    return {
        kind: typeof value.kind === "string" ? value.kind : undefined,
        ref: typeof value.ref === "string" ? value.ref : undefined,
        hash,
        size,
        contentType: typeof value.contentType === "string" ? value.contentType : undefined,
    };
}

function wfOutputReader(envelope) {
    const workflows = globalThis.__zs_env?.()?.workflows;
    const run = workflows?.[envelope.workflowName]?.get(String(envelope.runId ?? ""));
    if (typeof run?.readStepOutput !== "function") return undefined;
    return (name, occurrence) => run.readStepOutput(name, occurrence);
}

function wfOutputConfig(config) {
    if (!config || typeof config !== "object") return {};
    const output = config.output;
    if (typeof output === "string") return { outputMode: output };
    if (output && typeof output === "object") {
        return {
            ...(typeof output.as === "string" ? { outputMode: output.as } : {}),
            ...(typeof output.contentType === "string" && output.contentType
                ? { outputContentType: output.contentType }
                : {}),
        };
    }
    return {};
}

function wfHasCompensator(config) {
    return !!(config && typeof config === "object" && typeof config.compensate === "function");
}

function wfCreateStepOutputRef(descriptor, outputRead, runId, name, occurrence, memo) {
    const ref = descriptor.ref ?? `wfblob:sha256:${descriptor.hash}`;
    const memoKey = `${runId}:${name}:${occurrence}:${descriptor.hash}`;
    const readBytes = () => {
        let promise = memo.get(memoKey);
        if (!promise) {
            promise = wfReadStepOutputBytes(outputRead, name, occurrence);
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
        async json() {
            return JSON.parse(await readText());
        },
        async text() {
            return readText();
        },
        async arrayBuffer() {
            const bytes = await readBytes();
            return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
        },
        bytes() {
            return readBytes();
        },
        stream() {
            return new ReadableStream({
                async start(controller) {
                    controller.enqueue(await readBytes());
                    controller.close();
                },
            });
        },
    };
}

async function wfReadStepOutputBytes(outputRead, name, occurrence) {
    if (typeof outputRead !== "function") {
        throw wfErr("workflow output reader is unavailable", 500, "WORKFLOW_DEFINITION_ERROR");
    }
    return outputRead(name, occurrence);
}

// Runtime workflow drain barrier. The workflow SDK owns the related journal
// behavior.
class ZsDispatchMicrotaskQuiescenceBarrier {
    #version = 0;
    #stopped = false;

    markProgress() {
        this.#version++;
    }

    stop() {
        this.#stopped = true;
        this.markProgress();
    }

    waitUntilBlocked(isLegalPending) {
        return new Promise((_, reject) => {
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
                    reject(new ZsNondeterministicError(
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

class ZsJournalBackedStep {
    #stepsByOrdinal = new Map();
    #nameOccurrences = new Map();
    #quiescence;
    #cursor = 0;
    #frontier = undefined;
    #stepWorkObserved = false;
    #activeStepCallbacks = 0;
    #callbackSyncDepth = 0;
    #parallelIssueWindow = false;
    #parallelIssueWindowToken = 0;
    #runId = "";
    #outputRead = undefined;
    #outputReadMemo = new Map();
    #phase = "running";
    #trigger = {};
    #compensatorRegistry = new Map();
    #workflowNames;

    constructor(steps, quiescence, runId = "", outputRead = undefined, phase = "running", trigger = {}, workflowNames = new Map()) {
        this.#quiescence = quiescence;
        this.#runId = runId;
        this.#outputRead = outputRead;
        this.#phase = phase;
        this.#trigger = trigger;
        this.#workflowNames = workflowNames;
        for (const row of steps) this.#stepsByOrdinal.set(row.ordinal, row);
    }

    get frontierDrainPromise() {
        return this.#frontier?.drainPromise;
    }

    get frontierObserved() {
        return this.#stepWorkObserved || (this.#frontier?.observed ?? false);
    }

    get frontierPending() {
        return this.#frontier?.settled === false;
    }

    run(name, configOrFn, maybeFn) {
        this.#assertNotNested();
        const config = typeof configOrFn === "function" ? undefined : configOrFn;
        const fn = typeof configOrFn === "function" ? configOrFn : maybeFn;
        if (typeof fn !== "function") {
            return Promise.reject(wfErr("step.run requires a function body", 500, "WORKFLOW_DEFINITION_ERROR"));
        }
        const issued = this.#issue(name, "run");
        if (issued.record) {
            this.#registerCompensator(issued.record, config);
            return this.#recordPromise(issued.record);
        }
        return this.#registerFrontier(this.#runFrontier(issued, name, config, fn));
    }

    sideEffect(name, fn) {
        this.#assertNotNested();
        if (typeof fn !== "function") {
            return Promise.reject(wfErr("step.sideEffect requires a function body", 500, "WORKFLOW_DEFINITION_ERROR"));
        }
        const issued = this.#issue(name, "sideEffect");
        if (issued.record) return this.#recordPromise(issued.record);
        return this.#registerFrontier(this.#sideEffectFrontier(issued, name, fn));
    }

    sleep(name, duration) {
        this.#assertNotNested();
        const issued = this.#issue(name, "sleep");
        if (issued.record) {
            if (issued.record.state === "completed") return brandStepPromise(Promise.resolve());
            return this.#recordPromise(issued.record, {
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

    sleepUntil(name, when) {
        this.#assertNotNested();
        const target = typeof when === "number" ? new Date(when) : when;
        const issued = this.#issue(name, "sleep");
        if (issued.record) {
            if (issued.record.state === "completed") return brandStepPromise(Promise.resolve());
            return this.#recordPromise(issued.record, {
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

    waitForSignal(name, opts = {}) {
        this.#assertNotNested();
        const issued = this.#issue(name, "wait_signal");
        if (issued.record) {
            if (issued.record.state === "completed") {
                if (issued.record.output !== undefined) return brandStepPromise(Promise.resolve(issued.record.output));
                return brandStepPromise(Promise.resolve(issued.record.consumedSignal ?? null));
            }
            if (issued.record.state === "failed") return this.#recordPromise(issued.record);
            return this.#recordPromise(issued.record, {
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

    call(WorkflowClass, input, options) {
        this.#assertNotNested();
        const name = this.#workflowNames.get(WorkflowClass);
        if (name === undefined) {
            throw wfErr("child workflow must be an exported workflow constructor", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        if (name === null) {
            throw wfErr("workflow export bindings must be unambiguous", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        if (WorkflowClass.prototype == null || typeof WorkflowClass.prototype !== "object") {
            throw wfErr("child workflow must be an exported workflow constructor", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        const issued = this.#issue(name, "child");
        if (issued.record) return this.#recordPromise(issued.record);
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

    startMany(WorkflowClass, items, options) {
        this.#assertNotNested();
        const materialized = Array.from(items);
        if (materialized.length > ZS_MAX_START_MANY_BATCH) {
            return brandStepPromise(Promise.reject(new ZsLimitExceededError(
                `startMany batch exceeds maxStartManyBatch (${materialized.length} > ${ZS_MAX_START_MANY_BATCH})`,
            )));
        }
        return brandStepPromise(Promise.all(materialized.map((raw) => {
            const item = raw || {};
            const itemOptions = item.options && typeof item.options === "object" ? item.options : {};
            const mergedOptions = {
                ...(options && typeof options === "object" ? options : {}),
                ...itemOptions,
                ...(typeof item.key === "string" ? { key: item.key } : {}),
            };
            return this.call(WorkflowClass, item.input, mergedOptions);
        })));
    }

    continueAsNew(input) {
        this.#assertNotNested();
        throw new ZsWorkflowContinueAsNewSignal(input);
    }

    async #runFrontier(issued, name, config, fn) {
        const bodyPromise = this.#invokeStepBody(fn);
        try {
            const output = await bodyPromise;
            const outputConfig = wfOutputConfig(config);
            const compensable = wfHasCompensator(config);
            return {
                kind: "run",
                ordinal: issued.ordinal,
                name,
                nameOccurrence: issued.nameOccurrence,
                state: "completed",
                output,
                ...(compensable ? { compensable: true, compensationMaxAttempts: 1 } : {}),
                ...outputConfig,
            };
        } catch (e) {
            if (e instanceof ZsWorkflowContinueAsNewSignal) throw e;
            if (e instanceof ZsWorkflowSuspendSignal) throw e;
            return {
                kind: "run",
                ordinal: issued.ordinal,
                name,
                nameOccurrence: issued.nameOccurrence,
                state: "failed",
                error: wfSerializeError(e),
            };
        } finally {
            bodyPromise.catch(() => {});
            this.#activeStepCallbacks--;
            if (this.#activeStepCallbacks === 0) {
                this.#parallelIssueWindow = false;
            }
        }
    }

    async #sideEffectFrontier(issued, name, fn) {
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

    #invokeStepBody(fn) {
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
            return zsWorkflowDispatchAls.run({ mode: "step" }, () => Promise.resolve(fn()));
        } catch (e) {
            return Promise.reject(e);
        } finally {
            this.#callbackSyncDepth--;
        }
    }

    #suspendFrontier(outcome) {
        return this.#registerFrontier(Promise.resolve(outcome));
    }

    #recordPromise(record, pendingOutcome) {
        try {
            return brandStepPromise(
                Promise.resolve(this.#resolveRecord(record, pendingOutcome)),
                () => {
                    this.#stepWorkObserved = true;
                    this.#quiescence.markProgress();
                },
            );
        } catch (e) {
            if (e instanceof ZsWorkflowSuspendSignal) {
                return this.#registerFrontier(Promise.resolve(e.outcome));
            }
            return brandStepPromise(
                Promise.reject(e),
                () => {
                    this.#stepWorkObserved = true;
                    this.#quiescence.markProgress();
                },
            );
        }
    }

    #registerCompensator(record, config) {
        if (
            record.kind !== "run" ||
            record.state !== "completed" ||
            !wfHasCompensator(config)
        ) {
            return;
        }
        this.#compensatorRegistry.set(record.ordinal, {
            ordinal: record.ordinal,
            name: record.name,
            nameOccurrence: record.nameOccurrence ?? 0,
            output: this.#completedRecordValue(record),
            compensate: config.compensate,
            state: record.compensationState,
        });
    }

    #completedRecordValue(record) {
        if (record.outputRef) {
            return wfCreateStepOutputRef(
                record.outputRef,
                this.#outputRead,
                this.#runId,
                record.name,
                record.nameOccurrence ?? 0,
                this.#outputReadMemo,
            );
        }
        return record.output;
    }

    async runNextCompensator() {
        const pending = [...this.#compensatorRegistry.values()]
            .filter((entry) => entry.state === "pending" || entry.state === "running")
            .sort((a, b) => b.ordinal - a.ordinal)[0];
        if (!pending) {
            throw new ZsNondeterministicError("compensating run has no pending compensator");
        }
        const ctx = {
            idempotencyKey: `comp:${this.#runId}:${pending.ordinal}:${pending.nameOccurrence}`,
            trigger: this.#trigger,
        };
        try {
            await zsWorkflowDispatchAls.run(
                { mode: "step" },
                () => Promise.resolve(pending.compensate(pending.output, ctx)),
            );
            return {
                kind: "CompensationCompleted",
                ordinal: pending.ordinal,
                name: pending.name,
                nameOccurrence: pending.nameOccurrence,
            };
        } catch (e) {
            return {
                kind: "CompensationFailed",
                ordinal: pending.ordinal,
                name: pending.name,
                nameOccurrence: pending.nameOccurrence,
                error: wfSerializeError(e),
            };
        }
    }

    #registerFrontier(outcome) {
        const frontier = this.#frontier ??= new ZsFrontierCoordinator(this.#quiescence);
        if (!frontier.sealed) {
            frontier.add(outcome);
        }
        frontier.drainPromise.catch(() => {});
        wfSuppressUnhandledRejection(frontier.promise);
        return frontier.promise;
    }

    #issue(name, kind) {
        const ordinal = this.#cursor++;
        this.#quiescence.markProgress();
        const nameOccurrence = this.#nameOccurrences.get(name) ?? 0;
        this.#nameOccurrences.set(name, nameOccurrence + 1);
        const record = this.#stepsByOrdinal.get(ordinal);
        if (!record && this.#phase === "compensating") {
            throw new ZsWorkflowCompensationReplayReady();
        }
        if (record) {
            if (record.name !== name || record.kind !== kind || (record.nameOccurrence ?? 0) !== nameOccurrence) {
                throw new ZsNondeterministicError(
                    `workflow journal mismatch at ordinal ${ordinal}: expected ${kind} ${name}#${nameOccurrence}, got ${record.kind} ${record.name}#${record.nameOccurrence ?? 0}`,
                );
            }
        }
        return { ordinal, nameOccurrence, record };
    }

    #resolveRecord(record, pendingOutcome) {
        if (record.state === "completed") {
            return this.#completedRecordValue(record);
        }
        if (record.state === "failed") throw wfDeserializeError(record.error);
        if (this.#phase === "compensating") {
            throw new ZsWorkflowCompensationReplayReady();
        }
        throw new ZsWorkflowSuspendSignal(pendingOutcome ?? {
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
        });
    }

    #assertNotNested() {
        if (
            this.#activeStepCallbacks > 0 &&
            (this.#callbackSyncDepth > 0 || !this.#parallelIssueWindow)
        ) {
            throw wfErr("workflow step methods cannot be called from inside a step body", 500, "WORKFLOW_DEFINITION_ERROR");
        }
    }
}

class ZsFrontierCoordinator {
    promise;
    drainPromise;
    #quiescence;
    #pending = 0;
    #observed = false;
    #sealed = false;
    #settled = false;
    #fatal = undefined;
    #outcomes = [];
    #reject = () => {};

    constructor(quiescence) {
        this.#quiescence = quiescence;
        this.drainPromise = new Promise((_, reject) => {
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

    get sealed() {
        return this.#sealed;
    }

    get observed() {
        return this.#observed;
    }

    get settled() {
        return this.#settled;
    }

    add(outcome) {
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

    seal() {
        this.#quiescence.markProgress();
        if (!this.#observed) {
            this.#fatal ??= new ZsNondeterministicError(
                "workflow body awaited non-step work while a frontier was pending",
            );
        }
        this.#sealed = true;
        this.#maybeFinish();
    }

    #maybeFinish() {
        if (this.#settled || !this.#sealed || this.#pending > 0) return;
        this.#settled = true;
        if (this.#fatal !== undefined) {
            this.#reject(this.#fatal);
            return;
        }
        this.#outcomes.sort((a, b) => a.ordinal - b.ordinal);
        this.#reject(new ZsWorkflowSuspendSignal(this.#outcomes));
    }
}

const ZS_STEP_PROMISE_BRAND = Symbol.for("zeroship.workflow.stepPromise");

function isZsWorkflowStepPromise(value) {
    return (
        (typeof value === "object" || typeof value === "function") &&
        value !== null &&
        value[ZS_STEP_PROMISE_BRAND] === true
    );
}

class ZsWorkflowStepPromise extends Promise {
    #observed = false;
    #onObserve;

    static get [Symbol.species]() {
        return Promise;
    }

    constructor(executor, onObserve) {
        super(executor);
        this.#onObserve = onObserve;
        Object.defineProperty(this, ZS_STEP_PROMISE_BRAND, {
            value: true,
            configurable: false,
            enumerable: false,
            writable: false,
        });
    }

    then(onfulfilled, onrejected) {
        this.#observe();
        return super.then(onfulfilled, onrejected);
    }

    catch(onrejected) {
        this.#observe();
        return super.catch(onrejected);
    }

    finally(onfinally) {
        this.#observe();
        return super.finally(onfinally);
    }

    #observe() {
        if (this.#observed) return;
        this.#observed = true;
        this.#onObserve?.();
    }

    suppressUnhandledRejection() {
        super.then(undefined, () => {});
    }
}

function brandStepPromise(promise, onObserve) {
    if (isZsWorkflowStepPromise(promise)) return promise;
    return new ZsWorkflowStepPromise((resolve, reject) => {
        promise.then(resolve, reject);
    }, onObserve);
}

function wfSuppressUnhandledRejection(promise) {
    if (promise instanceof ZsWorkflowStepPromise) {
        promise.suppressUnhandledRejection();
        return;
    }
    promise.catch(() => {});
}

function workflowClasses(userNamespace) {
    const mod = userNamespace ?? {};
    const def = mod.default && typeof mod.default === "object" ? mod.default : {};
    const classes = new Map();
    const names = new Map();
    const add = (name, constructor) => {
        if (!name || typeof constructor !== "function") {
            throw wfErr("workflow export must name a constructor", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        if (classes.has(name) && classes.get(name) !== constructor) {
            const previous = classes.get(name);
            if (previous !== null) names.set(previous, null);
            names.set(constructor, null);
            classes.set(name, null);
        } else {
            classes.set(name, constructor);
            names.set(constructor, names.has(constructor) && names.get(constructor) !== name ? null : name);
        }
    };
    // Bind constructor identity before invoking app code. Minification, frozen
    // classes and later changes to Function.name cannot change child targets.
    // Inspecting prototype.run would lose valid instance-field implementations.
    // Unrelated callable exports may have aliases; reject ambiguity only when
    // resolving a workflow target, without constructing every exported value.
    for (const name of Object.keys(mod)) {
        if (name !== "default" && typeof mod[name] === "function") add(name, mod[name]);
    }
    const declared = def.workflows;
    if (declared != null) {
        if (typeof declared !== "object" || Array.isArray(declared)) {
            throw wfErr("default.workflows must be a workflow dictionary", 500, "WORKFLOW_DEFINITION_ERROR");
        }
        for (const name of Object.keys(declared)) add(name, declared[name]);
    }
    return { classes, names };
}

function resolveWorkflow(registry, workflowName) {
    const found = registry.classes.get(workflowName);
    if (found === undefined) throw wfErr("Workflow not found: " + workflowName, 404, "WORKFLOW_NOT_FOUND");
    if (found === null || registry.names.get(found) === null) {
        throw wfErr("workflow export bindings must be unambiguous", 500, "WORKFLOW_DEFINITION_ERROR");
    }
    return found;
}

function workflowFrontierResult(envelope, outcome) {
    const base = {
        runId: envelope.runId,
        nonce: envelope.nonce,
        workflowName: envelope.workflowName,
        ordinal: outcome.ordinal,
        name: outcome.name,
        nameOccurrence: outcome.nameOccurrence,
    };
    if ((outcome.kind === "run" || outcome.kind === "sideEffect") && outcome.state === "completed") {
        return {
            ...base,
            kind: "StepCompleted",
            stepKind: outcome.kind,
            output: outcome.output,
            ...(outcome.outputMode ? { outputMode: outcome.outputMode } : {}),
            ...(outcome.outputContentType ? { outputContentType: outcome.outputContentType } : {}),
            ...(outcome.compensable ? { compensable: true, compensationMaxAttempts: outcome.compensationMaxAttempts ?? 1 } : {}),
        };
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
        kind: "Child",
        childWorkflowName: outcome.workflowName,
        input: outcome.input,
        options: outcome.options,
    };
}

function workflowBatchResult(envelope, outcomes) {
    const mapped = outcomes.map((outcome) => workflowFrontierResult(envelope, outcome));
    const base = {
        runId: envelope.runId,
        dispatchNonce: envelope.nonce,
        workflowName: envelope.workflowName,
        outcomes: mapped,
    };
    return mapped.length === 1 ? { ...mapped[0], ...base } : base;
}

function workflowTerminalResult(envelope, outcome) {
    return {
        ...outcome,
        runId: envelope.runId,
        dispatchNonce: envelope.nonce,
        workflowName: envelope.workflowName,
        outcomes: [outcome],
    };
}

export async function dispatch(userNamespace, envelope, _ctx) {
    if (envelope == null || typeof envelope !== "object") {
        throw wfErr("workflow dispatch envelope must be an object", 400, "INVALID_ARGUMENT");
    }
    const workflowName = typeof envelope.workflowName === "string" ? envelope.workflowName : "";
    if (!workflowName) throw wfErr("workflowName is required", 400, "INVALID_ARGUMENT");
    const env = envelope;
    const terminalBatch = workflowTerminalResult;
    const resultBatch = workflowBatchResult;
    const ContinueAsNewSignal = ZsWorkflowContinueAsNewSignal;
    const SuspendSignal = ZsWorkflowSuspendSignal;
    try {
        const registry = workflowClasses(userNamespace);
        const WorkflowClass = resolveWorkflow(registry, workflowName);
        const workflow = new WorkflowClass();
        if (typeof workflow.run !== "function") {
            throw wfErr(`Workflow ${workflowName} has no run(trigger, step) method`, 500, "WORKFLOW_DEFINITION_ERROR");
        }
        const quiescence = new ZsDispatchMicrotaskQuiescenceBarrier();
        const trigger = wfTrigger(envelope);
        const step = new ZsJournalBackedStep(
            wfJournal(envelope),
            quiescence,
            String(envelope.runId ?? ""),
            wfOutputReader(envelope),
            String(envelope.phase ?? "running"),
            trigger,
            registry.names,
        );
        if (envelope.phase === "compensating") {
            try {
                await zsWorkflowDispatchAls.run(
                    { mode: "body" },
                    () => Promise.resolve(workflow.run(trigger, step)),
                );
            } catch (e) {
                if (
                    !(e instanceof ZsWorkflowCompensationReplayReady) &&
                    !(e instanceof ZsWorkflowSuspendSignal) &&
                    !(e instanceof ZsWorkflowContinueAsNewSignal)
                ) {
                    // Terminal forward errors are expected while rebuilding the registry.
                    // Corrupt prefixes still fail closed if no pending compensator
                    // reconstructs from the replayed journal.
                }
            }
            return workflowTerminalResult(envelope, await step.runNextCompensator());
        }
        const blockedByNonStepWork = quiescence.waitUntilBlocked(() => step.frontierObserved);
        let outputPromise;
        try {
            outputPromise = zsWorkflowDispatchAls.run(
                { mode: "body" },
                () => Promise.resolve(workflow.run(trigger, step)),
            );
        } catch (e) {
            quiescence.stop();
            blockedByNonStepWork.catch(() => {});
            step.frontierDrainPromise?.catch(() => {});
            throw e;
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
                        throw new ZsNondeterministicError("workflow completed while a frontier was pending");
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
            throw new ZsNondeterministicError("workflow completed while a frontier was pending");
        }
        return workflowTerminalResult(envelope, {
            kind: "RunCompleted",
            runId: envelope.runId,
            nonce: envelope.nonce,
            workflowName,
            output,
        });
    } catch (e) {
      if (e instanceof ContinueAsNewSignal) {
        return terminalBatch(env, {
          kind: "ContinueAsNew",
          runId: env.runId,
          nonce: env.nonce,
          workflowName,
          input: e.input,
        });
      }
      if (e instanceof SuspendSignal) {
        return resultBatch(env, e.outcomes);
      }
        return workflowTerminalResult(envelope, {
            kind: "RunFailed",
            runId: envelope.runId,
            nonce: envelope.nonce,
            workflowName,
            error: wfSerializeError(e),
        });
    }
}
