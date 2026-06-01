/**
 * PKCE transaction manager (Auth0 `TransactionManager` parity, gateway §4.3).
 *
 * Persists `{ verifier, state, nonce, redirect_uri }` for an in-flight
 * authorize→exchange flow under `@@zsauth@@::txn::<state>` in **sessionStorage**
 * so an opener reload mid-popup can still complete the exchange. A fast
 * in-memory mirror avoids a storage round-trip on the happy path; sessionStorage
 * is the durable source.
 *
 * The entry is cleared on completion, on a `state` mismatch, and on the 60 s
 * popup timeout (so stale transactions never accumulate or replay).
 */

import type { StorageLike } from "./env";

const TXN_PREFIX = "@@zsauth@@::txn::";

export interface Transaction {
  verifier: string;
  state: string;
  nonce: string;
  redirect_uri: string;
  /** Scopes requested for this flow (so the exchange can record them). */
  scopes: string[];
  /** Unix ms the transaction was created (for opportunistic GC). */
  createdAt: number;
}

export class TransactionManager {
  private readonly mem = new Map<string, Transaction>();

  constructor(private readonly session: StorageLike) {}

  private key(state: string): string {
    return TXN_PREFIX + state;
  }

  /** Persist a transaction to both the in-memory mirror and sessionStorage. */
  put(txn: Transaction): void {
    this.mem.set(txn.state, txn);
    try {
      this.session.setItem(this.key(txn.state), JSON.stringify(txn));
    } catch {
      // sessionStorage may be unavailable (privacy mode); the in-memory
      // mirror still carries the flow within a single page lifetime.
    }
  }

  /** Recover a transaction by `state` (memory first, then sessionStorage). */
  get(state: string): Transaction | null {
    const hit = this.mem.get(state);
    if (hit) return hit;
    try {
      const raw = this.session.getItem(this.key(state));
      if (!raw) return null;
      const txn = JSON.parse(raw) as Transaction;
      this.mem.set(state, txn);
      return txn;
    } catch {
      return null;
    }
  }

  /** Clear a single transaction (completion / mismatch / timeout). */
  remove(state: string): void {
    this.mem.delete(state);
    try {
      this.session.removeItem(this.key(state));
    } catch {
      // ignore — best effort
    }
  }

  /** All currently-persisted transaction states (for reload recovery). */
  pending(): Transaction[] {
    const out: Transaction[] = [];
    const seen = new Set<string>();
    for (const txn of this.mem.values()) {
      out.push(txn);
      seen.add(txn.state);
    }
    try {
      for (let i = 0; i < this.session.length; i++) {
        const k = this.session.key(i);
        if (!k || !k.startsWith(TXN_PREFIX)) continue;
        const state = k.slice(TXN_PREFIX.length);
        if (seen.has(state)) continue;
        const raw = this.session.getItem(k);
        if (!raw) continue;
        try {
          out.push(JSON.parse(raw) as Transaction);
        } catch {
          // skip corrupt entries
        }
      }
    } catch {
      // sessionStorage unavailable — return the in-memory view only
    }
    return out;
  }
}
