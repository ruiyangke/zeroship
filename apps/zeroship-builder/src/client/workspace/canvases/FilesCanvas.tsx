// ─── FilesCanvas — read-only manuscript view (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.2) ────────
//
// Crystal: rebuilt over @zeroship/ui. The two-region layout (file tree |
// viewer) rides the DS `Split` primitive (fixed 16rem rail + fluid viewer,
// collapsing to a stacked column on phones); the metadata that used to
// live in a third rail folds into the viewer footer as a DS
// `DescriptionList`.
//
// The tree is sourced from `listSandboxFiles({appId})` which proxies
// to the same sandbox Builder writes into via `getOrCreateSandboxFor`.
// The viewer shows the currently-selected file via
// `readSandboxFile({appId, path})` — read-only by design (Builder is
// the only writer; users edit by talking to the chat rail). The code is
// rendered with an app-local CodeMirror 6 view themed to crystal tokens.

import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import CodeMirror, { EditorView, type Extension } from "@uiw/react-codemirror";
import { javascript } from "@codemirror/lang-javascript";
import { json } from "@codemirror/lang-json";
import { css as cssLang } from "@codemirror/lang-css";
import { html } from "@codemirror/lang-html";
import {
  Banner,
  DescriptionList,
  EmptyState,
  ScrollArea,
  Spinner,
  Split,
} from "@zeroship/ui";
import { listSandboxFiles, readSandboxFile, type FileEntry } from "../../api";
import "./FilesCanvas.css";

// Language extensions for the CodeMirror viewer. Only the four installed
// `@codemirror/lang-*` packages are wired; everything else renders as
// plain text (still syntax-neutral but with line numbers + theming).
function langExtension(path: string): Extension[] {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  switch (ext) {
    case "ts":
    case "tsx":
      return [javascript({ jsx: true, typescript: true })];
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return [javascript({ jsx: true })];
    case "json":
      return [json()];
    case "css":
      return [cssLang()];
    case "html":
    case "htm":
      return [html()];
    default:
      return [];
  }
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
  useEffect(() => {
    if (selected || !tree.data || tree.data.length === 0) return;
    const first = tree.data.find((e) => e.kind === "file");
    if (first) setSelected(first.path);
  }, [selected, tree.data]);

  // Sandbox unreachable → gentle empty state. The proc itself throws
  // when the controller isn't running; surface that as a nudge rather
  // than a red error band.
  if (tree.error) {
    return (
      <div
        data-testid="files-canvas"
        className="zs-files zs-files--empty"
      >
        <EmptyState
          className="zs-files__sandbox-down"
          title="Sandbox not running."
          description={
            <>
              The sandbox controller isn't reachable. Start it with{" "}
              <code className="zs-files__code">cd crates/sandbox &amp;&amp; cargo run</code>{" "}
              and refresh this canvas.
            </>
          }
        />
      </div>
    );
  }

  return (
    <div data-testid="files-canvas" className="zs-files">
      <Split
        side="start"
        sideWidth="16rem"
        collapseBelow="md"
        className="zs-files__split"
      >
        <Split.Side className="zs-files__side">
          <FileTree
            entries={tree.data ?? []}
            selected={selected}
            onSelect={setSelected}
            loading={tree.isLoading}
          />
        </Split.Side>
        <Split.Main className="zs-files__main">
          <FileViewer
            path={selected}
            content={file.data}
            loading={file.isLoading}
            error={file.error}
          />
        </Split.Main>
      </Split>
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
    <aside className="zs-files__tree">
      <div className="zs-files__tree-label">Manuscript</div>
      <ScrollArea className="zs-files__tree-scroll">
        {loading && (
          <div className="zs-files__hint zs-files__tree-hint">
            <Spinner size="sm" />
            <span>loading…</span>
          </div>
        )}
        {!loading && entries.length === 0 && (
          <div
            data-testid="files-empty"
            className="zs-files__hint zs-files__tree-hint"
          >
            Builder hasn't written anything yet — start a turn in the chat
            and the manuscript will fill in here.
          </div>
        )}
        <ul className="zs-files__list">
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
      </ScrollArea>
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
  // Indent is driven by an inline custom property the .css multiplies
  // against the per-step indent token — no raw px in the markup.
  const indentVar = { "--zs-files-depth": String(depth) } as React.CSSProperties;

  if (node.kind === "dir") {
    return (
      <li>
        <button
          type="button"
          onClick={() => setOpen(!open)}
          data-testid={`file-tree-item:${node.path}`}
          className="zs-files__row zs-files__row--dir"
          style={indentVar}
          aria-expanded={open}
        >
          <span className="zs-files__chevron" aria-hidden="true">
            {open ? "▾" : "▸"}
          </span>
          <span className="zs-files__row-name">{node.name}</span>
        </button>
        {open && node.children.length > 0 && (
          <ul className="zs-files__list">
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
        className="zs-files__row zs-files__row--file"
        data-active={isActive ? "" : undefined}
        aria-current={isActive ? "true" : undefined}
        style={indentVar}
      >
        <span className="zs-files__chevron" aria-hidden="true" />
        <span className="zs-files__row-name">{node.name}</span>
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
  return (
    <div className="zs-files__viewer">
      <div className="zs-files__viewer-head">
        <span className="zs-files__viewer-path">{path ?? "—"}</span>
        <span className="zs-files__viewer-tag">read-only</span>
      </div>
      <div className="zs-files__viewer-body" data-testid="files-viewer">
        {!path ? (
          <div className="zs-files__hint">Pick a file to read it.</div>
        ) : loading && !content ? (
          <div className="zs-files__hint">
            <Spinner size="sm" />
            <span>loading…</span>
          </div>
        ) : error ? (
          <Banner intent="danger" className="zs-files__error">
            <Banner.Description>couldn't read file</Banner.Description>
          </Banner>
        ) : (
          <CodeView code={content ?? ""} path={path} />
        )}
      </div>
      <FileMeta path={path} content={content} />
    </div>
  );
}

// Crystal CodeMirror theme — values reference `--zs-*` tokens directly;
// the runtime resolves the custom properties through the injected
// stylesheet, so the editor tracks the active crystal theme. System
// colors / token vars only — no raw hex.
const crystalEditorTheme = EditorView.theme({
  "&": {
    backgroundColor: "transparent",
    color: "var(--zs-label)",
    fontSize: "var(--zs-text-footnote-size)",
    height: "100%",
  },
  ".cm-scroller": {
    fontFamily: "var(--zs-font-mono)",
    lineHeight: "1.65",
  },
  ".cm-content": {
    caretColor: "var(--zs-accent)",
  },
  ".cm-gutters": {
    backgroundColor: "transparent",
    color: "var(--zs-label-quaternary)",
    border: "none",
  },
  ".cm-lineNumbers .cm-gutterElement": {
    color: "var(--zs-label-quaternary)",
    padding: "0 var(--zs-space-3) 0 var(--zs-space-1)",
  },
  ".cm-activeLine": {
    backgroundColor: "var(--zs-fill-quaternary)",
  },
  ".cm-activeLineGutter": {
    backgroundColor: "transparent",
    color: "var(--zs-label-secondary)",
  },
  "&.cm-focused": {
    outline: "none",
  },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--zs-fill)",
  },
});

function CodeView({ code, path }: { code: string; path: string }) {
  const extensions = useMemo(() => langExtension(path), [path]);
  return (
    <CodeMirror
      value={code}
      readOnly
      editable={false}
      theme={crystalEditorTheme}
      extensions={extensions}
      className="zs-files__codemirror"
      basicSetup={{
        lineNumbers: true,
        foldGutter: false,
        highlightActiveLine: false,
        highlightActiveLineGutter: false,
        searchKeymap: false,
        autocompletion: false,
      }}
    />
  );
}

// ─── meta (viewer footer) ────────────────────────────────────────

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
    <div className="zs-files__meta">
      <DescriptionList orientation="horizontal" className="zs-files__meta-list">
        <DescriptionList.Item>
          <DescriptionList.Term>Size</DescriptionList.Term>
          <DescriptionList.Detail>{size}</DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>Lines</DescriptionList.Term>
          <DescriptionList.Detail>{lines}</DescriptionList.Detail>
        </DescriptionList.Item>
      </DescriptionList>
      <span className="zs-files__meta-note">
        Builder is the only writer. Tell the chat what to change.
      </span>
    </div>
  );
}
