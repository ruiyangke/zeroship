// ─── storage — graceful localStorage wrappers ───────────────────
//
// Private-browsing mode (Safari) and SSR contexts can throw on
// localStorage access. Every read/write the onboarding + lifecycle
// surfaces do goes through these helpers so the UI never crashes
// when the storage backend is missing or misbehaving.
//
// Keep this dumb on purpose — no fancy schema, just key/value with
// try/catch. Callers handle null = "not set" themselves.

export function lsGet(key: string): string | null {
  try {
    return typeof localStorage !== "undefined" ? localStorage.getItem(key) : null;
  } catch {
    return null;
  }
}

export function lsSet(key: string, value: string): void {
  try {
    if (typeof localStorage !== "undefined") localStorage.setItem(key, value);
  } catch {
    // Quota exceeded / disabled — best effort.
  }
}

export function lsRemove(key: string): void {
  try {
    if (typeof localStorage !== "undefined") localStorage.removeItem(key);
  } catch {
    // Best effort.
  }
}
