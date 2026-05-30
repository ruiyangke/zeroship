/**
 * Cross-tab refresh serialization (gateway §3/§4.3).
 *
 * Wraps `navigator.locks.request('zs.refresh.<host>', {signal: AbortController(5s)}, fn)`.
 * When the Web Locks API is unavailable, a same-isolate in-process fallback
 * serializes refreshes within the tab (a `browser-tabs-lock`-style chain). The
 * fallback cannot serialize across tabs, but the gateway's per-anchor mint
 * single-flight already coalesces concurrent server-side mints, so the worst
 * case is one redundant `/session?mint=1` — never a correctness bug.
 */

import type { LockManagerLike } from "./env";

const LOCK_TIMEOUT_MS = 5000;

export class RefreshLock {
  /** In-process fallback chain (one tab). Replaced atomically per acquisition. */
  private chain: Promise<unknown> = Promise.resolve();

  constructor(
    private readonly name: string,
    private readonly locks?: LockManagerLike,
  ) {}

  /**
   * Run `fn` under the refresh lock. With the Web Locks API the lock is named
   * (cross-tab); otherwise the in-process chain serializes within the tab.
   * Aborts the lock acquisition after 5 s so a wedged holder cannot deadlock
   * a refresh forever (the callback still runs once the lock is granted).
   */
  async run<T>(fn: () => Promise<T>): Promise<T> {
    if (this.locks) {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), LOCK_TIMEOUT_MS);
      try {
        return await this.locks.request(this.name, { signal: controller.signal }, fn);
      } finally {
        clearTimeout(timer);
      }
    }
    // Fallback: chain on the previous acquisition so only one runs at a time.
    const prior = this.chain;
    let release!: () => void;
    this.chain = new Promise<void>((resolve) => {
      release = resolve;
    });
    try {
      await prior;
      return await fn();
    } finally {
      release();
    }
  }
}
