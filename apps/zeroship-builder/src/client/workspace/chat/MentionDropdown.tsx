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
// Crystal: re-skinned over `--zs-*` tokens via the co-located
// MentionDropdown.css. We deliberately keep the controlled listbox
// markup (plain ul/li/button with click handlers, no portal, no
// third-party combobox lib) rather than reaching for the DS
// Popover/Combobox/Autocomplete — those are trigger-driven and own
// their open-state + keyboard rover, which would fight this
// component's contract: the parent composer owns open-state, the
// query, the active index, mousedown selection, and forwards keyboard
// events itself, and positions the panel above its own textarea.
// The skin mirrors the DS Menu popup idiom (opaque surface sheet,
// popover shadow, accent-tinted active row) without taking on the DS
// popover's self-contained behavior.

import { useEffect, useRef, useState } from "react";
import {
  listSandboxFiles,
  listIssues,
  getLogs,
  type FileEntry,
  type Issue,
} from "../../api";
import "./MentionDropdown.css";

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
          const lines = await getLogs(appId);
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
    <div data-testid="mention-dropdown" role="listbox" className="zs-mention">
      <div className="zs-mention__head">
        <span>{kind === "file" ? "files" : kind === "issue" ? "issues" : "recent error"}</span>
        <span className="zs-mention__legend">@file · @issue · @recent</span>
      </div>
      {loading && <div className="zs-mention__hint">searching…</div>}
      {!loading && items.length === 0 && (
        <div className="zs-mention__hint">no matches</div>
      )}
      <ul className="zs-mention__list">
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
              className="zs-mention__item"
            >
              <span className="zs-mention__label">{item.label}</span>
              {item.sub && <span className="zs-mention__sub">{item.sub}</span>}
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
