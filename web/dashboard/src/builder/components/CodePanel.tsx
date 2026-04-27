// ─── CodePanel ──────────────────────────────────────────────────
// CodeMirror 6 view of the deployed `server.js`. Read-only by
// default — the agent owns the code; the human can flip to "edit"
// to make manual tweaks (deploy is a separate explicit click).

import { useState, useMemo, useEffect } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { javascript } from "@codemirror/lang-javascript";
import { oneDark } from "@codemirror/theme-one-dark";
import { EditorView } from "@codemirror/view";
import { Button } from "@/components/ui/button";
import { Loader2, Save, Pencil, Eye, AlertCircle } from "lucide-react";

interface Props {
  /** Latest code (from server fetch). Resets local editor on change. */
  code: string;
  /** True while the parent is saving — disables the Save button. */
  saving: boolean;
  /** Optional last-error to display at the bottom of the panel. */
  error: string | null;
  /** Called when the human clicks Save. */
  onSave: (next: string) => void;
}

export function CodePanel({ code, saving, error, onSave }: Props) {
  const [editable, setEditable] = useState(false);
  const [draft, setDraft] = useState(code);

  // Reset draft whenever the upstream code changes (e.g., agent re-deploy).
  useEffect(() => {
    setDraft(code);
  }, [code]);

  const dirty = draft !== code;

  const extensions = useMemo(
    () => [
      javascript({ typescript: false, jsx: false }),
      EditorView.theme({
        "&": { fontSize: "12.5px", height: "100%" },
        ".cm-scroller": { fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace" },
      }),
      EditorView.editable.of(editable),
    ],
    [editable],
  );

  return (
    <div className="h-full flex flex-col bg-background border-l border-border">
      <div className="border-b border-border bg-muted/30 px-3 py-1.5 flex items-center gap-2">
        <span className="text-xs font-mono text-muted-foreground flex-1">
          server.js
          {dirty && <span className="ml-2 text-amber-500">●</span>}
        </span>

        <Button
          type="button"
          variant="ghost"
          onClick={() => setEditable(!editable)}
          title={editable ? "Switch to read-only" : "Enable editing"}
          className="h-6 w-6 p-0"
        >
          {editable ? <Eye className="size-3" /> : <Pencil className="size-3" />}
        </Button>

        {editable && dirty && (
          <Button
            type="button"
            variant="primary"
            disabled={saving}
            onClick={() => onSave(draft)}
            className="h-6 px-2 text-xs gap-1"
          >
            {saving ? <Loader2 className="size-3 animate-spin" /> : <Save className="size-3" />}
            {saving ? "saving" : "deploy"}
          </Button>
        )}
      </div>

      <div className="flex-1 min-h-0 overflow-hidden">
        <CodeMirror
          value={draft}
          theme={oneDark}
          extensions={extensions}
          basicSetup={{
            lineNumbers: true,
            highlightActiveLine: editable,
            foldGutter: true,
            bracketMatching: true,
            closeBrackets: editable,
            autocompletion: editable,
            highlightSelectionMatches: false,
          }}
          onChange={(value) => setDraft(value)}
          height="100%"
          style={{ height: "100%" }}
        />
      </div>

      {error && (
        <div className="border-t border-destructive/40 bg-destructive/10 text-destructive px-3 py-1.5 text-xs flex items-center gap-1.5">
          <AlertCircle className="size-3" />
          {error}
        </div>
      )}
    </div>
  );
}
