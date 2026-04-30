// ─── FilesTab — the manuscript view ─────────────────────────────
//
// Tree on the left, file in focus center, marginalia (size, path,
// modified) in the right rail. Code in mono, framing in serif.

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { listProjectFiles, readProjectFile } from "../../api/files";
import { useWorkspace } from "../ProjectWorkspace";
import { TabDrawer } from "../components/TabDrawer";

export function FilesTab() {
  const { appId } = useWorkspace();
  const [selected, setSelected] = useState<string | null>(null);

  const tree = useQuery({
    queryKey: ["files", appId],
    queryFn: () => listProjectFiles(appId),
  });

  const file = useQuery({
    queryKey: ["file", appId, selected],
    queryFn: () => selected ? readProjectFile(appId, selected) : Promise.resolve(""),
    enabled: !!selected,
  });

  // Auto-select first file once the tree loads
  if (!selected && tree.data && tree.data.length > 0) {
    setSelected(tree.data[0].path);
  }

  return (
    <div className="h-full flex flex-col" data-testid="files-tab">
      <div className="flex-1 grid min-h-0" style={{ gridTemplateColumns: "240px 1fr 200px" }}>
        <aside className="border-r border-rule bg-paper-2 p-4 overflow-auto font-serif text-[14px]">
          <div className="label-uc mb-2">Manuscript</div>
          {tree.isLoading && <div className="font-serif italic text-pencil">loading…</div>}
          {tree.error && <div className="font-serif italic text-tomato">couldn't list files</div>}
          {tree.data?.length === 0 && (
            <div className="font-serif italic text-pencil">no files yet</div>
          )}
          <ul className="list-none p-0 m-0">
            {tree.data?.map((entry) => {
              const isActive = entry.path === selected;
              return (
                <li key={entry.path} className="py-0.5">
                  <button
                    type="button"
                    onClick={() => setSelected(entry.path)}
                    className={
                      "w-full text-left bg-transparent border-0 cursor-pointer font-serif px-1 py-0.5 " +
                      (isActive
                        ? "text-ink font-medium"
                        : "text-ink-soft hover:text-ink")
                    }
                    style={isActive ? { borderLeft: "2px solid var(--color-tomato)", paddingLeft: "10px", marginLeft: "-12px" } : undefined}
                  >
                    {entry.path}
                  </button>
                </li>
              );
            })}
          </ul>
        </aside>

        <div className="bg-white p-5 overflow-auto font-mono text-[13px] leading-[1.65] relative" data-testid="files-editor">
          {!selected ? (
            <div className="font-serif italic text-pencil">Pick a file from the manuscript to read it.</div>
          ) : file.isLoading ? (
            <div className="font-serif italic text-pencil">loading…</div>
          ) : file.error ? (
            <div className="font-serif italic text-tomato">couldn't read file</div>
          ) : (
            <CodeView code={file.data ?? ""} />
          )}
        </div>

        <aside className="border-l border-rule bg-paper p-5 overflow-auto">
          <div className="label-uc mb-1.5">Currently open</div>
          <dl className="m-0 space-y-3">
            <DlRow label="Path" value={selected ?? "—"} mono />
            <DlRow
              label="Size"
              value={file.data ? `${file.data.length} chars · ${file.data.split("\n").length} lines` : "—"}
            />
            <DlRow label="Modified" value={<em className="italic">just now</em>} />
            <DlRow label="Author" value={<em className="italic">The studio</em>} />
          </dl>
          <div className="label-uc mt-7 mb-1.5">Project</div>
          <dl className="m-0 space-y-3">
            <DlRow label="Files" value={String(tree.data?.length ?? "—")} />
          </dl>
        </aside>
      </div>
      <TabDrawer appId={appId} />
    </div>
  );
}

function CodeView({ code }: { code: string }) {
  const lines = code.split("\n");
  return (
    <div>
      {lines.map((line, i) => (
        <div key={i} className="whitespace-pre">
          <span
            className="inline-block w-7 pr-3 text-right select-none font-mono text-[12px]"
            style={{ color: "var(--color-tomato)", opacity: 0.55 }}
          >
            {i + 1}
          </span>
          {line || " "}
        </div>
      ))}
    </div>
  );
}

function DlRow({ label, value, mono }: { label: string; value: React.ReactNode; mono?: boolean }) {
  return (
    <div>
      <dt className="font-sans text-[10px] uppercase tracking-[0.16em] text-pencil">{label}</dt>
      <dd className={"mt-0.5 m-0 " + (mono ? "font-mono text-[11.5px] text-ink" : "font-serif text-[13px] text-ink")}>
        {value}
      </dd>
    </div>
  );
}
