// ─── FilesTab — file tree + CodeMirror ──────────────────────────
// File tree on the left, CodeMirror 6 editor on the right.
// Open files become tabs across the top. Saving (cmd-s or button)
// PUTs to the agent's /projects/:id/files/* proxy and bumps the
// workspace deploy version so the preview reloads.

import { useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import CodeMirror from "@uiw/react-codemirror";
import { javascript } from "@codemirror/lang-javascript";
import { html } from "@codemirror/lang-html";
import { css } from "@codemirror/lang-css";
import { json } from "@codemirror/lang-json";
import { oneDark } from "@codemirror/theme-one-dark";
import { EditorView, keymap } from "@codemirror/view";
import { Loader2, Save, X, RefreshCw, AlertCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useWorkspace } from "../ProjectWorkspace";
import {
  listProjectFiles, readProjectFile, writeProjectFile,
  languageFor,
} from "../../api/files";
import { FileTree } from "../components/FileTree";

interface OpenTab {
  path: string;
  /** content as last fetched / saved */
  saved: string;
  /** current editor buffer */
  draft: string;
}

export function FilesTab() {
  const { appId, bumpDeployVersion } = useWorkspace();
  const queryClient = useQueryClient();
  const [tabs, setTabs] = useState<OpenTab[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  // File-tree query.
  const { data: entries, isLoading, error: listError, refetch } = useQuery({
    queryKey: ["sandbox-files", appId],
    queryFn: () => listProjectFiles(appId),
    refetchOnWindowFocus: false,
  });

  // Open a file: fetch it, push as a tab, focus it.
  async function openFile(path: string) {
    const existing = tabs.find((t) => t.path === path);
    if (existing) {
      setActive(path);
      return;
    }
    try {
      const content = await readProjectFile(appId, path);
      setTabs((prev) => [...prev, { path, saved: content, draft: content }]);
      setActive(path);
    } catch (e: any) {
      setSaveError(e?.message ?? String(e));
    }
  }

  function closeTab(path: string) {
    setTabs((prev) => prev.filter((t) => t.path !== path));
    if (active === path) {
      const next = tabs.findIndex((t) => t.path === path);
      const fallback = tabs[next - 1] ?? tabs[next + 1];
      setActive(fallback?.path ?? null);
    }
  }

  function updateDraft(path: string, draft: string) {
    setTabs((prev) => prev.map((t) => (t.path === path ? { ...t, draft } : t)));
  }

  async function save(path: string) {
    const tab = tabs.find((t) => t.path === path);
    if (!tab || tab.draft === tab.saved) return;
    setSaving(true); setSaveError(null);
    try {
      await writeProjectFile(appId, path, tab.draft);
      setTabs((prev) => prev.map((t) => (t.path === path ? { ...t, saved: t.draft } : t)));
      // Re-list the tree in case the save created a new file.
      queryClient.invalidateQueries({ queryKey: ["sandbox-files", appId] });
      // Tell the workspace to reload the preview iframe — file change
      // alone won't trigger redeploy, but the UX feels right.
      bumpDeployVersion();
    } catch (e: any) {
      setSaveError(e?.message ?? String(e));
    } finally {
      setSaving(false);
    }
  }

  const activeTab = tabs.find((t) => t.path === active) ?? null;

  return (
    <div data-testid="files-tab" className="h-full grid grid-cols-[240px_1fr] min-h-0">
      <aside className="border-r border-border min-h-0 flex flex-col">
        <div className="px-3 py-1.5 border-b border-border bg-muted/30 flex items-center gap-2 text-xs font-mono text-muted-foreground">
          <span className="flex-1 truncate">workspace</span>
          <Button
            type="button" variant="ghost" className="h-6 w-6 p-0"
            onClick={() => refetch()}
            title="Refresh"
          >
            <RefreshCw className={`size-3 ${isLoading ? "animate-spin" : ""}`} />
          </Button>
        </div>
        <div className="flex-1 overflow-auto py-1">
          {isLoading && <div className="text-xs text-muted-foreground px-3 py-2">loading…</div>}
          {listError && (
            <div className="text-xs text-destructive px-3 py-2 flex items-center gap-1.5">
              <AlertCircle className="size-3" />
              {(listError as Error).message}
            </div>
          )}
          {entries && entries.length === 0 && (
            <div className="text-xs text-muted-foreground italic px-3 py-2">empty workspace</div>
          )}
          {entries && entries.length > 0 && (
            <FileTree entries={entries} selected={active} onSelect={openFile} />
          )}
        </div>
      </aside>

      <section className="min-h-0 min-w-0 flex flex-col">
        {/* Open-file tab strip */}
        <div className="flex items-stretch border-b border-border bg-muted/20 overflow-x-auto">
          {tabs.length === 0 ? (
            <div className="px-3 py-2 text-xs text-muted-foreground italic">
              click a file in the tree to open it
            </div>
          ) : (
            tabs.map((t) => (
              <div
                key={t.path}
                className={`group inline-flex items-center gap-1.5 px-3 py-1.5 text-xs font-mono border-r border-border cursor-pointer ${
                  active === t.path
                    ? "bg-background text-foreground"
                    : "bg-transparent text-muted-foreground hover:text-foreground"
                }`}
                onClick={() => setActive(t.path)}
                data-testid={`files-tab-open:${t.path}`}
              >
                <span className="truncate max-w-[200px]">{t.path}</span>
                {t.draft !== t.saved && <span className="size-1.5 rounded-full bg-amber-500" />}
                <button
                  type="button"
                  className="opacity-50 group-hover:opacity-100 hover:text-destructive"
                  onClick={(e) => { e.stopPropagation(); closeTab(t.path); }}
                  title="Close"
                >
                  <X className="size-3" />
                </button>
              </div>
            ))
          )}
        </div>

        {/* Editor */}
        <div className="flex-1 min-h-0 min-w-0 overflow-hidden">
          {activeTab ? (
            <Editor
              key={activeTab.path}
              path={activeTab.path}
              value={activeTab.draft}
              dirty={activeTab.draft !== activeTab.saved}
              saving={saving}
              onChange={(v) => updateDraft(activeTab.path, v)}
              onSave={() => save(activeTab.path)}
            />
          ) : (
            <div className="h-full flex items-center justify-center text-xs text-muted-foreground">
              no file open
            </div>
          )}
        </div>

        {saveError && (
          <div className="border-t border-destructive/40 bg-destructive/10 text-destructive px-3 py-1.5 text-xs flex items-center gap-1.5">
            <AlertCircle className="size-3" />
            {saveError}
          </div>
        )}
      </section>
    </div>
  );
}

// ─── Editor ─────────────────────────────────────────────────────

interface EditorProps {
  path: string;
  value: string;
  dirty: boolean;
  saving: boolean;
  onChange: (v: string) => void;
  onSave: () => void;
}

function Editor({ path, value, dirty, saving, onChange, onSave }: EditorProps) {
  const onSaveRef = useRef(onSave);
  onSaveRef.current = onSave;

  const extensions = useMemo(() => {
    const langExt = (() => {
      switch (languageFor(path)) {
        case "javascript": return javascript({ jsx: true, typescript: true });
        case "html":       return html();
        case "css":        return css();
        case "json":       return json();
        default:           return null;
      }
    })();

    return [
      ...(langExt ? [langExt] : []),
      EditorView.theme({
        "&": { fontSize: "12.5px", height: "100%" },
        ".cm-scroller": { fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace" },
      }),
      keymap.of([
        {
          key: "Mod-s",
          run: () => { onSaveRef.current(); return true; },
        },
      ]),
    ];
  }, [path]);

  return (
    <div className="h-full flex flex-col">
      <div className="border-b border-border bg-muted/30 px-3 py-1 flex items-center gap-2 text-xs font-mono text-muted-foreground">
        <span className="flex-1">{path}</span>
        {dirty && (
          <Button
            type="button" variant="primary"
            disabled={saving}
            onClick={onSave}
            className="h-6 px-2 text-xs gap-1"
            data-testid="files-save"
          >
            {saving ? <Loader2 className="size-3 animate-spin" /> : <Save className="size-3" />}
            {saving ? "saving" : "save (⌘S)"}
          </Button>
        )}
      </div>
      <div className="flex-1 min-h-0 overflow-hidden">
        <CodeMirror
          value={value}
          theme={oneDark}
          extensions={extensions}
          onChange={onChange}
          height="100%"
          style={{ height: "100%" }}
          basicSetup={{
            lineNumbers: true,
            highlightActiveLine: true,
            foldGutter: true,
            bracketMatching: true,
            closeBrackets: true,
            autocompletion: true,
            highlightSelectionMatches: false,
          }}
        />
      </div>
    </div>
  );
}
