import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";

/**
 * A piece of state that lives in the URL's query string.
 *
 * The bug list's filters were component state, so a narrowed list was
 * something you could look at and not something you could send: no bookmark,
 * no link in a chat, no Back to undo a filter, and a reload dropped it. That
 * is the half of routing the hash never made worth doing -- now that the app
 * has real URLs, the URL may as well say what you are looking at.
 *
 * Only QUERY state belongs here. Which columns are shown and whether a menu is
 * open are about this browser, not about this list, and they stay local (the
 * column choice is already remembered in localStorage).
 *
 * `replace` is the difference between a filter you can undo and a history
 * stack full of keystrokes. Discrete choices -- a status, a page -- push, so
 * Back steps out of them one at a time. Typing replaces, or every character
 * in the search box would be its own entry.
 */
export function useQueryParam(
  key: string,
  fallback = "",
  { replace = false }: { replace?: boolean } = {},
): [string, (next: string) => void] {
  const [params, setParams] = useSearchParams();
  const value = params.get(key) ?? fallback;

  const set = useCallback(
    (next: string) => {
      setParams(
        () => {
          // Built from the LIVE url, not from the params this render closed
          // over. Two filters changed in quick succession both read the same
          // starting value otherwise, and the second write drops the first --
          // observed as a product filter vanishing when a severity was chosen
          // straight after it. react-router pushes to history synchronously,
          // so window.location is current even before React re-renders.
          const out = new URLSearchParams(window.location.search);
          // An absent parameter reads as the fallback, so writing the fallback
          // means deleting it. Otherwise a "cleared" filter would still show
          // up in the URL as `?status=` and travel with every link.
          if (!next || next === fallback) out.delete(key);
          else out.set(key, next);
          // Any change to WHAT is being asked returns to the first page.
          // Keeping the offset would show an empty table for a query that
          // matched plenty, which reads as "no results" rather than "page 4".
          if (key !== "offset") out.delete("offset");
          return out;
        },
        { replace },
      );
    },
    [key, fallback, replace, setParams],
  );

  return [value, set];
}

/** The same, for a value the caller keeps as a number. */
export function useNumericQueryParam(
  key: string,
  fallback: number,
): [number, (next: number) => void] {
  const [raw, setRaw] = useQueryParam(key, String(fallback));
  const parsed = Number(raw);
  return [
    Number.isFinite(parsed) ? parsed : fallback,
    useCallback((next: number) => setRaw(String(next)), [setRaw]),
  ];
}

/**
 * Clear every query parameter in ONE write.
 *
 * Not six setter calls in a row. Each one reads the params it was rendered
 * with, so a handler that calls them in sequence hands the same starting value
 * to all six and only the last survives -- "Clear filters" would drop one
 * filter and keep the rest, which looks like the button half-works.
 */
export function useClearQuery(): () => void {
  const [, setParams] = useSearchParams();
  return useCallback(() => setParams(new URLSearchParams()), [setParams]);
}
