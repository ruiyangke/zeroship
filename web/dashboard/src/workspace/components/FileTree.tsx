// ─── FileTree ───────────────────────────────────────────────────
// Compact directory-collapsed view of the workspace. Click a file
// to open it in the editor; click a dir to expand/collapse.
// We build the tree in-memory from the flat list the sandbox API
// returns (it walks recursively but skips noise like node_modules).

import { useMemo, useState } from "react";
import { ChevronRight, File, Folder, FolderOpen } from "lucide-react";
import { cn } from "@/lib/utils";
import type { FileEntry } from "../../api/files";

interface Props {
  entries: FileEntry[];
  selected: string | null;
  onSelect: (path: string) => void;
}

interface Node {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  children: Node[];
}

function buildTree(entries: FileEntry[]): Node {
  const root: Node = { name: "", path: "", isDir: true, size: 0, children: [] };
  for (const e of entries) {
    const parts = e.path.split("/");
    let cur = root;
    for (let i = 0; i < parts.length; i++) {
      const name = parts[i];
      const isLast = i === parts.length - 1;
      let child = cur.children.find((c) => c.name === name);
      if (!child) {
        child = {
          name,
          path: parts.slice(0, i + 1).join("/"),
          isDir: !isLast || e.kind === "dir",
          size: isLast ? e.size : 0,
          children: [],
        };
        cur.children.push(child);
      }
      cur = child;
    }
  }
  // Sort: directories first, then alphabetic.
  const sort = (n: Node) => {
    n.children.sort((a, b) => {
      if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
    n.children.forEach(sort);
  };
  sort(root);
  return root;
}

export function FileTree({ entries, selected, onSelect }: Props) {
  const root = useMemo(() => buildTree(entries), [entries]);
  return (
    <div className="text-xs font-mono select-none" data-testid="file-tree">
      {root.children.map((c) => (
        <TreeNode key={c.path} node={c} depth={0} selected={selected} onSelect={onSelect} />
      ))}
    </div>
  );
}

function TreeNode({
  node, depth, selected, onSelect,
}: { node: Node; depth: number; selected: string | null; onSelect: (p: string) => void }) {
  const [open, setOpen] = useState(depth < 1);
  const isSelected = selected === node.path;

  if (!node.isDir) {
    return (
      <button
        type="button"
        onClick={() => onSelect(node.path)}
        data-testid={`file-tree-file:${node.path}`}
        className={cn(
          "flex items-center gap-1.5 w-full text-left px-2 py-0.5 hover:bg-muted/50 transition-colors",
          isSelected && "bg-primary/10 text-primary",
        )}
        style={{ paddingLeft: depth * 12 + 8 }}
      >
        <File className="size-3 shrink-0 text-muted-foreground" />
        <span className="truncate flex-1">{node.name}</span>
      </button>
    );
  }

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex items-center gap-1 w-full text-left px-2 py-0.5 hover:bg-muted/50 transition-colors"
        style={{ paddingLeft: depth * 12 + 4 }}
      >
        <ChevronRight className={cn("size-3 shrink-0 transition-transform", open && "rotate-90")} />
        {open ? <FolderOpen className="size-3 text-muted-foreground" /> : <Folder className="size-3 text-muted-foreground" />}
        <span className="truncate flex-1">{node.name}</span>
      </button>
      {open && node.children.map((c) => (
        <TreeNode key={c.path} node={c} depth={depth + 1} selected={selected} onSelect={onSelect} />
      ))}
    </>
  );
}
