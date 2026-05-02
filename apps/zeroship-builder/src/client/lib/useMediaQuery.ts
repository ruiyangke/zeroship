// ─── useMediaQuery — a tiny matchMedia hook ─────────────────────
//
// Subscribe to a CSS media query and re-render when its match state
// flips. The first paint reads the current value synchronously so
// SSR-hydration and initial render agree (no flash of "wrong layout").
//
// Used by WorkspaceShell to collapse the chat sidebar into a drawer
// on phones (< 768px). Generic enough for any future breakpoint use
// without dragging in a 4kB library.

import { useEffect, useState } from "react";

/** Returns `true` while `query` matches. SSR-safe (defaults to false
 *  when `window` is missing). */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState<boolean>(() => {
    if (typeof window === "undefined" || !window.matchMedia) return false;
    return window.matchMedia(query).matches;
  });

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mql = window.matchMedia(query);
    const onChange = (e: MediaQueryListEvent) => setMatches(e.matches);
    // Sync on mount in case the query changed before the listener
    // attached (e.g. rotation between render and effect run).
    setMatches(mql.matches);
    mql.addEventListener("change", onChange);
    return () => mql.removeEventListener("change", onChange);
  }, [query]);

  return matches;
}

/** Convenience wrapper — true on phones (< 768px). */
export function useIsPhone(): boolean {
  return useMediaQuery("(max-width: 767px)");
}
