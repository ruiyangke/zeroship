// `@`-mention dropdown for the ChatComposer (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §10.2 / §10 plan).
//
// Three context kinds:
//   - file   → autocomplete file paths from the sandbox (`listSandboxFiles`).
//   - issue  → autocomplete issue titles for this app (`listIssues`).
//   - recent → paste the first error line from `getAppLogs`.
//
// Trigger: a bare `@` typed at the start of the composer or after
// whitespace. The composer detects the trigger position, opens the
// dropdown anchored just above the textarea, and forwards keyboard
// events (↑/↓/Enter/Tab/Esc) so the user never leaves the keyboard.
// Selecting an item splices the rendered string into the textarea
// at the trigger position (replacing the `@…` token).
//
// We render plain ul/li with click handlers — Tailwind tokens, no
// portal, no third-party combobox lib. The popover is positioned by
// the parent (the composer absolutely positions us above its
// textarea so it tracks resize / scrolls).

import { useEffect, useRef, useState } from "react";
import {
  listSandboxFiles,
  listIssues,
  getAppLogs,
  type FileEntry,
  type Issue,
} from "../../api";
import { cn } from "../../lib/utils";

export type MentionItem =
  | { kind: "file"; label: string; insert: string; sub?: string }
  | { kind: "issue"; label: string; insert: string; sub?: string }
  | { kind: "recent"; label: string; insert: string; sub?: string };

export interface MentionDropdownProps {
  /** App id from the route. When missing, suggestions are empty. */
  appId?: string;
  /** Free-text query after the `@` (excluding the `@` itself). */
  query: string;
  /** Index of the currently active item (kept in the parent so the
   *  composer's keyboard handler can drive ↑/↓ without re-querying
   *  the dropdown's internal state). */
  activeIndex: number;
  /** Setter for the active index (composer needs to clamp on suggest
   *  list changes). */
  onActiveIndexChange: (i: number) => void;
  /** Selecting an item commits its `insert` string at the trigger
   *  position. The composer handles the splice. */
  onSelect: (item: MentionItem) => void;
  /** Called when the suggestions list changes — composer uses it to
   *  re-clamp `activeIndex` if the new list is shorter. */
  onItemsChange?: (items: MentionItem[]) => void;
}

/**
 * Build the menu's items for a given query. The first character of the
 * query selects the kind: `f` → files, `i` → issues, `r` → recent
 * errors. A bare `@` (no leading char) defaults to files (the most
 * common case) and shows a small footer hint about the other kinds.
 */
function classifyQuery(query: string): {
  kind: "file" | "issue" | "recent";
  filter: string;
} {
  const trimmed = query.trim();
  if (trimmed.startsWith("issue ")) {
    return { kind: "issue", filter: trimmed.slice("issue ".length) };
  }
  if (trimmed === "issue") return { kind: "issue", filter: "" };
  if (trimmed.startsWith("recent")) return { kind: "recent", filter: "" };
  if (trimmed.startsWith("file ")) {
    return { kind: "file", filter: trimmed.slice("file ".length) };
  }
  if (trimmed === "file") return { kind: "file", filter: "" };
  // Bare query: treat as file path filter.
  return { kind: "file", filter: trimmed };
}

export function MentionDropdown({
  appId,
  query,
  activeIndex,
  onActiveIndexChange,
  onSelect,
  onItemsChange,
}: MentionDropdownProps) {
  const [items, setItems] = useState<MentionItem[]>([]);
  const [loading, setLoading] = useState(false);
  // Stable ref so the async fetch can no-op if the query has changed
  // by the time it resolves. Cheaper than wiring AbortController
  // through three RPC procs that don't currently accept signals.
  const tokenRef = useRef(0);

  const { kind, filter } = classifyQuery(query);

  useEffect(() => {
    if (!appId) {
      setItems([]);
      onItemsChange?.([]);
      return;
    }
    const myToken = ++tokenRef.current;
    setLoading(true);
    void (async () => {
      try {
        let next: MentionItem[] = [];
        if (kind === "file") {
          const files = await listSandboxFiles({ appId });
          next = filterFiles(files, filter).map((f) => ({
            kind: "file" as const,
            label: f.path,
            insert: `@file ${f.path}`,
            sub: f.kind === "dir" ? "directory" : `${f.size} B`,
          }));
        } else if (kind === "issue") {
          const { issues } = await listIssues({ appId });
          next = filterIssues(issues, filter).map((i) => ({
            kind: "issue" as const,
            label: i.title,
            insert: `@issue ${i.title}`,
            sub: i.status,
          }));
        } else {
          // recent — peek at the latest log lines and pick the first
          // error-flavoured one. Falls back to the most recent line if
          // nothing matches.
          const lines = await getAppLogs(appId);
          const errored = lines.find((l) => /error|fail|panic|exception/i.test(l));
          const pick = errored ?? lines[lines.length - 1];
          if (pick) {
            next = [
              {
                kind: "recent" as const,
                label: pick.length > 70 ? pick.slice(0, 70) + "…" : pick,
                insert: `@recent ${pick}`,
                sub: errored ? "error" : "latest log",
              },
            ];
          }
        }
        if (myToken !== tokenRef.current) return;
        setItems(next);
        onItemsChange?.(next);
      } catch {
        if (myToken !== tokenRef.current) return;
        setItems([]);
        onItemsChange?.([]);
      } finally {
        if (myToken === tokenRef.current) setLoading(false);
      }
    })();
    // We intentionally exclude onItemsChange from deps — it's stable
    // by usage convention (composer wraps it in useCallback). Listing
    // it would cause an extra fetch round whenever the parent
    // re-renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId, kind, filter]);

  return (
    <div
      data-testid="mention-dropdown"
      role="listbox"
      className={cn(
        "absolute left-3 right-3 bottom-full mb-1 z-20",
        "bg-paper border border-ink/40 rounded shadow-lg",
        "max-h-56 overflow-y-auto",
      )}
    >
      <div className="px-3 py-1.5 border-b border-rule font-sans text-[10px] uppercase tracking-wider text-pencil flex items-center justify-between">
        <span>{kind === "file" ? "files" : kind === "issue" ? "issues" : "recent error"}</span>
        <span className="text-pencil/70">@file · @issue · @recent</span>
      </div>
      {loading && (
        <div className="px-3 py-2 font-serif italic text-pencil text-[12.5px]">
          searching…
        </div>
      )}
      {!loading && items.length === 0 && (
        <div className="px-3 py-2 font-serif italic text-pencil text-[12.5px]">
          no matches
        </div>
      )}
      <ul className="py-1">
        {items.map((item, i) => (
          <li key={`${item.kind}-${item.insert}`}>
            <button
              type="button"
              data-testid={`mention-item-${i}`}
              role="option"
              aria-selected={i === activeIndex}
              // mousedown rather than click — the textarea would lose
              // focus on click and mouseup might race the blur handler
              // in the composer. mousedown fires before blur and we
              // preventDefault to keep focus pinned.
              onMouseDown={(e) => {
                e.preventDefault();
                onSelect(item);
              }}
              onMouseEnter={() => onActiveIndexChange(i)}
              className={cn(
                "w-full text-left px-3 py-1.5 cursor-pointer",
                "font-mono text-[12px] flex items-center justify-between gap-3",
                i === activeIndex
                  ? "bg-tomato/10 text-ink"
                  : "text-ink-soft hover:bg-paper-2",
              )}
            >
              <span className="truncate">{item.label}</span>
              {item.sub && (
                <span className="font-sans text-[10px] uppercase tracking-wider text-pencil shrink-0">
                  {item.sub}
                </span>
              )}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

function filterFiles(files: FileEntry[], filter: string): FileEntry[] {
  // Drop noisy directories that the LLM doesn't care about anyway.
  // Keeps the dropdown focused on user-authored source.
  const denied = /^(node_modules|dist|build|\.git|\.cache)\b/;
  const f = filter.toLowerCase();
  const matches = files
    .filter((e) => !denied.test(e.path))
    .filter((e) => (f ? e.path.toLowerCase().includes(f) : true))
    // files first, then dirs
    .sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === "file" ? -1 : 1;
      return a.path.localeCompare(b.path);
    });
  return matches.slice(0, 10);
}

function filterIssues(issues: Issue[], filter: string): Issue[] {
  const f = filter.toLowerCase();
  return issues
    .filter((i) => (f ? i.title.toLowerCase().includes(f) : true))
    .slice(0, 10);
}
