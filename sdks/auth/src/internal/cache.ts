/**
 * Token cache (Auth0 `CacheManager` parity, gateway §4.3).
 *
 *   - `ICache` (re-exported from ./types) is the pluggable backend.
 *   - `InMemoryCache` (default) is a closure over a plain object — no token at
 *     rest.
 *   - `LocalStorageCache` is opt-in (documented XSS tradeoff).
 *   - `CacheManager` sits above the backend and owns KEYING + EXPIRY. Token
 *     entries key on `clientId::audience::scope` (here `<appOrigin>::<scope-sorted>`);
 *     the user-profile entry is a SEPARATE key. On read it evicts expired
 *     entries unless a refresh token is present.
 */

import type { ICache, Session, User } from "../types";

const PREFIX = "@@zsauth@@";

/** Default in-memory backend: a closure over a plain object, no token at rest. */
export class InMemoryCache implements ICache {
  private store: Record<string, unknown> = Object.create(null);

  get<T>(key: string): T | undefined {
    return this.store[key] as T | undefined;
  }
  set<T>(key: string, value: T): void {
    this.store[key] = value;
  }
  remove(key: string): void {
    delete this.store[key];
  }
  allKeys(): string[] {
    return Object.keys(this.store);
  }
}

/** Opt-in localStorage backend (survives reloads; documented XSS tradeoff). */
export class LocalStorageCache implements ICache {
  constructor(private readonly storage: Pick<Storage, "getItem" | "setItem" | "removeItem" | "key" | "length">) {}

  get<T>(key: string): T | undefined {
    const raw = this.storage.getItem(key);
    if (raw == null) return undefined;
    try {
      return JSON.parse(raw) as T;
    } catch {
      return undefined;
    }
  }
  set<T>(key: string, value: T): void {
    this.storage.setItem(key, JSON.stringify(value));
  }
  remove(key: string): void {
    this.storage.removeItem(key);
  }
  allKeys(): string[] {
    const out: string[] = [];
    for (let i = 0; i < this.storage.length; i++) {
      const k = this.storage.key(i);
      if (k != null && k.startsWith(PREFIX)) out.push(k);
    }
    return out;
  }
}

/** The persisted token entry. `expiresAt` is unix seconds. */
export interface CacheEntry {
  access_token: string;
  id_token?: string;
  refresh_token?: string;
  token_type: "Bearer";
  /** Unix seconds. */
  expiresAt: number;
  user: User;
  scopes: string[];
  appOrigin: string;
}

/** Stable scope key: sorted + space-joined so order never splits an entry. */
function scopeKey(scopes: string[]): string {
  return [...scopes].sort().join(" ");
}

export class CacheManager {
  constructor(
    private readonly cache: ICache,
    private readonly appOrigin: string,
  ) {}

  private tokenKey(scopes: string[]): string {
    return `${PREFIX}::${this.appOrigin}::${scopeKey(scopes)}`;
  }
  private userKey(): string {
    return `${PREFIX}::${this.appOrigin}::@@user@@`;
  }

  /**
   * Read the token entry for `scopes`, evicting it when expired UNLESS a
   * refresh token is present (so a silent renewal can still fire). Returns
   * `undefined` on miss/eviction.
   */
  async getEntry(scopes: string[], nowSecs: number): Promise<CacheEntry | undefined> {
    const key = this.tokenKey(scopes);
    const entry = (await this.cache.get<CacheEntry>(key)) ?? undefined;
    if (!entry) return undefined;
    if (entry.expiresAt <= nowSecs && !entry.refresh_token) {
      await this.cache.remove(key);
      return undefined;
    }
    return entry;
  }

  /** Persist a session as a token entry + separate user-profile entry. */
  async setSession(session: Session): Promise<void> {
    const entry: CacheEntry = {
      access_token: session.access_token,
      refresh_token: session.refresh_token,
      token_type: session.token_type,
      expiresAt: session.expires_at,
      user: session.user,
      scopes: session.scopes,
      appOrigin: this.appOrigin,
    };
    await this.cache.set(this.tokenKey(session.scopes), entry);
    await this.cache.set(this.userKey(), session.user);
  }

  /** The server-validated user profile (separate entry). */
  async getUser(): Promise<User | undefined> {
    return (await this.cache.get<User>(this.userKey())) ?? undefined;
  }

  async setUser(user: User): Promise<void> {
    await this.cache.set(this.userKey(), user);
  }

  /** Rebuild a `Session` from a cached entry. */
  static toSession(entry: CacheEntry): Session {
    return {
      access_token: entry.access_token,
      refresh_token: entry.refresh_token,
      expires_at: entry.expiresAt,
      token_type: entry.token_type,
      user: entry.user,
      scopes: entry.scopes,
    };
  }

  /** Drop EVERY entry for this app (sign-out). */
  async clear(): Promise<void> {
    const keys = (await this.cache.allKeys?.()) ?? [];
    const mine = keys.filter((k) => k.startsWith(`${PREFIX}::${this.appOrigin}::`));
    for (const k of mine) await this.cache.remove(k);
    // Belt-and-suspenders when the backend has no allKeys: remove the user key.
    await this.cache.remove(this.userKey());
  }
}
