// StreamPacer keeps a long, CPU-bound JS loop from monopolising the isolate's
// pump. The runtime polices that loop with `record_pump_cpu` in
// crates/zeroship-runtime/src/core/runtime.rs: the pump-side V8 time spent in
// timer callbacks and promise continuations is accumulated and compared against
// the wall time of a rolling window, and an isolate that spends too large a
// share of that window in JS is terminated. That guard runs on every plan,
// including `unlimited`, and is separate from the per-request CPU budget, which
// is charged in CPU time; once a streaming loop's per-chunk work moves into a
// promise continuation it is governed by the pump guard instead.
//
// The pacer measures only the synchronous JS sections the caller wraps, so the
// time it pauses is proportional to real work. Time spent awaiting storage is
// never measured -- it never enters the accumulator and so never lengthens a
// pause. Once the accumulated work crosses `thresholdMs`, it sleeps for that
// same measured amount; pausing for the work just done keeps the JS share of
// the window at half. `now` and `sleep` are injectable so the accounting can be
// unit-tested without a real clock.

/** Monotonic clock reading, in milliseconds. */
export type Clock = () => number;

/** Suspends the current async context for `ms` milliseconds. */
export type Sleep = (ms: number) => Promise<void>;

const DEFAULT_THRESHOLD_MS = 100;

export type StreamPacerOptions = {
  now?: Clock;
  sleep?: Sleep;
  thresholdMs?: number;
};

export class StreamPacer {
  private readonly now: Clock;
  private readonly sleep: Sleep;
  private readonly thresholdMs: number;
  private accumulatedMs = 0;

  constructor(options: StreamPacerOptions = {}) {
    this.now = options.now ?? (() => performance.now());
    this.sleep =
      options.sleep ??
      ((ms) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
    this.thresholdMs = options.thresholdMs ?? DEFAULT_THRESHOLD_MS;
  }

  /**
   * Run `work` as one measured synchronous section. Its duration alone is added
   * to the accumulator; once that crosses the threshold the pacer sleeps for
   * exactly the accumulated work and resets. A caller that awaits storage
   * between sections contributes nothing to the pause.
   */
  async work(work: () => void): Promise<void> {
    const startedAt = this.now();
    work();
    this.accumulatedMs += this.now() - startedAt;
    if (this.accumulatedMs < this.thresholdMs) return;
    const pauseMs = this.accumulatedMs;
    this.accumulatedMs = 0;
    await this.sleep(pauseMs);
  }
}
