// ─── FilesCanvas — read-only manuscript view (spec §9.2) ────────
//
// 3-column layout:
//   [ tree (260px) | content (flex-1) | metadata (200px) ]
//
// The tree is sourced from `listSandboxFiles({appId})` which proxies
// to the same sandbox Builder writes into via `getOrCreateSandboxFor`.
// The center pane shows the currently-selected file via
// `readSandboxFile({appId, path})` — read-only by design (Builder is
// the only writer; users edit by talking to the chat rail).

import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { listSandboxFiles, readSandboxFile, type FileEntry } from "../../api";

// ─── shiki: lazy-loaded singleton highlighter ──────────────────────
//
// We import shiki only when the first file opens, then cache the
// `codeToHtml` function for the rest of the session. Loading it eagerly
// would pull a multi-MB WASM blob into the initial bundle for users who
// never touch the files canvas.

type CodeToHtml = (
  code: string,
  options: { lang: string; theme: string },
) => Promise<string>;

let shikiPromise: Promise<CodeToHtml> | null = null;

function loadShiki(): Promise<CodeToHtml> {
  if (!shikiPromise) {
    shikiPromise = import("shiki").then((m) => m.codeToHtml as CodeToHtml);
  }
  return shikiPromise;
}

const EXT_TO_LANG: Record<string, string> = {
  ts: "typescript",
  tsx: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  py: "python",
  rs: "rust",
  md: "markdown",
  mdx: "markdown",
  json: "json",
  css: "css",
  html: "html",
  htm: "html",
  yml: "yaml",
  yaml: "yaml",
  toml: "toml",
  sh: "shell",
  bash: "shell",
};

function langFromPath(path: string): string {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return EXT_TO_LANG[ext] ?? "text";
}

export interface FilesCanvasProps {
  appId: string;
}

export function FilesCanvas({ appId }: FilesCanvasProps) {
  const [selected, setSelected] = useState<string | null>(null);

  const tree = useQuery({
    queryKey: ["sandbox-files", appId],
    queryFn: () => listSandboxFiles({ appId }),
    refetchInterval: 5000,
    retry: false,
  });

  const file = useQuery({
    queryKey: ["sandbox-file", appId, selected],
    queryFn: () => selected
      ? readSandboxFile({ appId, path: selected })
      : Promise.resolve(""),
    enabled: !!selected,
    retry: false,
  });

  // Auto-select first regular file once the tree resolves so the
  // viewer isn't a blank rectangle on first paint.
  if (!selected && tree.data && tree.data.length > 0) {
    const first = tree.data.find((e) => e.kind === "file");
    if (first) setSelected(first.path);
  }

  // Sandbox unreachable → editorial empty state. The proc itself
  // throws when the controller isn't running; surface that as a
  // gentle nudge rather than a red error band.
  if (tree.error) {
    return (
      <div
        data-testid="files-canvas"
        className="h-full flex items-center justify-center bg-paper-2"
      >
        <div className="max-w-md text-center px-6">
          <h3 className="font-display text-2xl font-medium text-ink mb-2">
            <em className="italic">Sandbox not running.</em>
          </h3>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            The sandbox controller isn't reachable. Start it with{" "}
            <code className="font-mono text-[12px] bg-paper px-1 py-0.5 rounded-[2px] border border-rule">
              cd crates/sandbox && cargo run
            </code>{" "}
            and refresh this canvas.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div
      data-testid="files-canvas"
      // Phone: stack tree on top of viewer, hide meta rail (the same
      // info is visible in the file's first lines + footer copy).
      // Tablet+: 2-col tree + viewer. Desktop (lg+): full 3-col with meta.
      className="h-full grid min-h-0 bg-paper grid-cols-1 md:grid-cols-[220px_1fr] lg:grid-cols-[260px_1fr_200px]"
    >
      <FileTree
        entries={tree.data ?? []}
        selected={selected}
        onSelect={setSelected}
        loading={tree.isLoading}
      />
      <FileViewer
        path={selected}
        content={file.data}
        loading={file.isLoading}
        error={file.error}
      />
      <div className="hidden lg:block">
        <FileMeta path={selected} content={file.data} />
      </div>
    </div>
  );
}

// ─── tree (left rail) ────────────────────────────────────────────

interface TreeNode {
  name: string;
  path: string;
  kind: "file" | "dir";
  size: number;
  children: TreeNode[];
}

function buildTree(entries: FileEntry[]): TreeNode[] {
  // The controller returns a flat list; reshape into a nested tree.
  // `kind: "dir"` entries pre-create the folders even when their
  // children come later in the list, so the tree resolves cleanly
  // regardless of input order.
  const root: TreeNode = { name: "", path: "", kind: "dir", size: 0, children: [] };
  const dirs = new Map<string, TreeNode>([["", root]]);

  // Sort: dirs before files at each level, alphabetical within.
  const sorted = [...entries].sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
    return a.path.localeCompare(b.path);
  });

  for (const e of sorted) {
    const segments = e.path.split("/");
    const name = segments[segments.length - 1] ?? e.path;
    const parentPath = segments.slice(0, -1).join("/");
    let parent = dirs.get(parentPath);
    if (!parent) {
      // Create intermediate directories that the flat list omitted.
      let pathSoFar = "";
      parent = root;
      for (const seg of segments.slice(0, -1)) {
        pathSoFar = pathSoFar ? `${pathSoFar}/${seg}` : seg;
        let next = dirs.get(pathSoFar);
        if (!next) {
          next = { name: seg, path: pathSoFar, kind: "dir", size: 0, children: [] };
          parent.children.push(next);
          dirs.set(pathSoFar, next);
        }
        parent = next;
      }
    }
    const node: TreeNode = { name, path: e.path, kind: e.kind, size: e.size, children: [] };
    if (e.kind === "dir") {
      // Replace any pre-created stub with the real entry (preserve
      // any children already attached to the stub).
      const existing = dirs.get(e.path);
      if (existing) {
        existing.size = e.size;
        continue;
      }
      dirs.set(e.path, node);
    }
    parent.children.push(node);
  }
  return root.children;
}

function FileTree({
  entries,
  selected,
  onSelect,
  loading,
}: {
  entries: FileEntry[];
  selected: string | null;
  onSelect: (path: string) => void;
  loading: boolean;
}) {
  const tree = useMemo(() => buildTree(entries), [entries]);

  return (
    <aside className="border-r border-rule bg-paper-2 overflow-auto p-4">
      <div className="label-uc mb-3">Manuscript</div>
      {loading && (
        <div className="font-serif italic text-pencil text-[13px]">loading…</div>
      )}
      {!loading && entries.length === 0 && (
        <div
          data-testid="files-empty"
          className="font-serif italic text-pencil text-[13px] leading-[1.55]"
        >
          Builder hasn't written anything yet — start a turn in the chat
          and the manuscript will fill in here.
        </div>
      )}
      <ul className="list-none p-0 m-0 font-serif text-[13.5px]">
        {tree.map((node) => (
          <TreeRow
            key={node.path}
            node={node}
            depth={0}
            selected={selected}
            onSelect={onSelect}
          />
        ))}
      </ul>
    </aside>
  );
}

function TreeRow({
  node,
  depth,
  selected,
  onSelect,
}: {
  node: TreeNode;
  depth: number;
  selected: string | null;
  onSelect: (path: string) => void;
}) {
  // Folders default open at the top level; deeper folders default
  // closed so a dump of node_modules-equivalent trees doesn't
  // explode. (The controller's file_tree already filters obvious
  // noise, but defence-in-depth.)
  const [open, setOpen] = useState(depth === 0);
  const isActive = node.path === selected;
  const indent = { paddingLeft: `${depth * 12 + 4}px` };

  if (node.kind === "dir") {
    return (
      <li>
        <button
          type="button"
          onClick={() => setOpen(!open)}
          data-testid={`file-tree-item:${node.path}`}
          className="w-full text-left bg-transparent border-0 cursor-pointer py-0.5 text-ink-soft hover:text-ink font-serif"
          style={indent}
        >
          <span className="inline-block w-3 text-pencil text-[10px]">
            {open ? "▾" : "▸"}
          </span>{" "}
          {node.name}
        </button>
        {open && node.children.length > 0 && (
          <ul className="list-none p-0 m-0">
            {node.children.map((c) => (
              <TreeRow
                key={c.path}
                node={c}
                depth={depth + 1}
                selected={selected}
                onSelect={onSelect}
              />
            ))}
          </ul>
        )}
      </li>
    );
  }

  return (
    <li>
      <button
        type="button"
        onClick={() => onSelect(node.path)}
        data-testid={`file-tree-item:${node.path}`}
        className={
          "w-full text-left bg-transparent border-0 cursor-pointer py-0.5 font-serif " +
          (isActive ? "text-ink font-medium" : "text-ink-soft hover:text-ink")
        }
        style={
          isActive
            ? {
                ...indent,
                borderLeft: "2px solid var(--color-tomato)",
                paddingLeft: `${depth * 12 + 2}px`,
              }
            : indent
        }
      >
        <span className="inline-block w-3" /> {node.name}
      </button>
    </li>
  );
}

// ─── viewer (centre) ─────────────────────────────────────────────

function FileViewer({
  path,
  content,
  loading,
  error,
}: {
  path: string | null;
  content: string | undefined;
  loading: boolean;
  error: unknown;
}) {
  // `min-w-0` is the key to making this column shrinkable inside the
  // grid — without it, long source lines force the column to expand
  // past its `1fr` share and clip the metadata rail. The inner content
  // wrapper also needs `min-w-0` so the shiki <pre>'s overflow-x can
  // actually take effect (shiki sets `white-space: pre`, so without a
  // width constraint above it, the pre never scrolls).
  return (
    <div className="bg-white flex flex-col min-h-0 min-w-0 overflow-hidden">
      <div className="border-b border-rule px-5 py-2.5 flex items-center justify-between bg-white">
        <span className="font-mono text-[12px] text-ink-soft truncate">
          {path ?? "—"}
        </span>
        <span className="font-serif italic text-[11.5px] text-pencil shrink-0 ml-3">
          read-only
        </span>
      </div>
      <div
        className="flex-1 min-h-0 min-w-0 overflow-auto px-5 py-4 font-mono text-[12.5px] leading-[1.65]"
        data-testid="files-viewer"
      >
        {!path ? (
          <div className="font-serif italic text-pencil text-[14px]">
            Pick a file to read it.
          </div>
        ) : loading && !content ? (
          <div className="font-serif italic text-pencil text-[14px]">
            loading…
          </div>
        ) : error ? (
          <div className="font-serif italic text-tomato text-[14px]">
            couldn't read file
          </div>
        ) : (
          <CodeView code={content ?? ""} path={path} />
        )}
      </div>
      <div className="border-t border-rule px-5 py-2 font-serif italic text-[12px] text-pencil shrink-0">
        Builder is the only writer. Tell the chat what to change.
      </div>
    </div>
  );
}

function CodeView({ code, path }: { code: string; path: string }) {
  // Plain (un-highlighted) line-numbered view used for the "text" lang
  // and as the synchronous fallback while shiki spins up its WASM.
  const fallback = (
    <div>
      {code.split("\n").map((line, i) => (
        <div key={i} className="whitespace-pre">
          <span
            className="inline-block w-9 pr-3 text-right select-none font-mono text-[11px]"
            style={{ color: "var(--color-tomato)", opacity: 0.5 }}
          >
            {i + 1}
          </span>
          {line || " "}
        </div>
      ))}
    </div>
  );

  const lang = useMemo(() => langFromPath(path), [path]);
  // Track which (code, lang) pair the cached `html` belongs to. When
  // the user switches files we WANT the new content to render
  // immediately (raw text, instant) — the highlighted HTML belongs to
  // the previous file and would mislead. So we render the fallback
  // until shiki finishes, but we don't blank `html` while a new shiki
  // job runs against THE SAME content (defence against StrictMode
  // double-effect or component re-mounts mid-fetch).
  const [highlight, setHighlight] = useState<{
    code: string;
    lang: string;
    html: string;
  } | null>(null);

  useEffect(() => {
    if (lang === "text" || !code) {
      setHighlight(null);
      return;
    }
    let cancelled = false;
    loadShiki()
      .then((codeToHtml) =>
        codeToHtml(code, { lang, theme: "github-light" }),
      )
      .then((rendered) => {
        if (!cancelled) setHighlight({ code, lang, html: rendered });
      })
      .catch(() => {
        // Highlighting failed (unsupported lang, WASM blocked, etc).
        // Leave `highlight` stale so the fallback renders the new code.
      });
    return () => {
      cancelled = true;
    };
  }, [code, lang]);

  // Two-phase render: ALWAYS show the fallback (raw, line-numbered
  // text) the instant `code` arrives. If shiki has finished and its
  // result matches the current code/lang, swap to the highlighted
  // overlay. While shiki is still working on the current code, the
  // fallback stays visible — no spinner, no flash.
  const ready =
    highlight && highlight.code === code && highlight.lang === lang;
  if (!ready) return fallback;

  // shiki returns a <pre><code>…</code></pre> wrapper. We let it apply
  // its own colours/background, but reset its margins and pin our font
  // sizing so it harmonises with the rest of the canvas.
  return (
    <div
      className="shiki-host font-mono text-[12.5px] leading-[1.65]"
      data-lang={lang}
      // shiki produces trusted HTML from a static theme; safe to inject.
      dangerouslySetInnerHTML={{ __html: highlight.html }}
    />
  );
}

// ─── meta (right rail) ───────────────────────────────────────────

function FileMeta({
  path,
  content,
}: {
  path: string | null;
  content: string | undefined;
}) {
  const size = content ? `${content.length} chars` : "—";
  const lines = content ? `${content.split("\n").length} lines` : "—";
  return (
    <aside className="border-l border-rule bg-paper p-5 overflow-auto">
      <div className="label-uc mb-2">Currently open</div>
      <dl className="m-0 space-y-3">
        <DlRow label="Path" value={path ?? "—"} mono />
        <DlRow label="Size" value={size} />
        <DlRow label="Lines" value={lines} />
        <DlRow label="Modified" value={<em className="italic">just now</em>} />
      </dl>
    </aside>
  );
}

function DlRow({
  label,
  value,
  mono,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div>
      <dt className="font-sans text-[10px] uppercase tracking-[0.16em] text-pencil">
        {label}
      </dt>
      <dd
        className={
          "mt-0.5 m-0 break-words " +
          (mono ? "font-mono text-[11.5px] text-ink" : "font-serif text-[13px] text-ink")
        }
      >
        {value}
      </dd>
    </div>
  );
}
