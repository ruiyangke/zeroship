import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "../../components/Button";
import { cn } from "../../lib/utils";
import { MessageActions } from "./MessageActions";

export interface MessageUserProps {
  text: string;
  time?: string;
  attachments?: ReactNode;
  /** Called with the new text when the user saves an edit. ChatRail
   *  truncates the conversation after this turn, then sends the new
   *  text as a fresh user message (replaying from this point). */
  onEdit?: (newText: string) => void;
}

export function MessageUser({ text, time, attachments, onEdit }: MessageUserProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(text);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Re-sync the draft if the parent's `text` prop changes while we're
  // not editing (e.g. message replay rebuilds the array). Avoids stale
  // text leaking into the edit field on the next pencil-click.
  useEffect(() => {
    if (!editing) setDraft(text);
  }, [text, editing]);

  // Autofocus + select-all when entering edit mode so ⌘A isn't needed.
  useEffect(() => {
    if (editing && textareaRef.current) {
      textareaRef.current.focus();
      textareaRef.current.select();
    }
  }, [editing]);

  function startEdit() {
    setDraft(text);
    setEditing(true);
  }

  function cancel() {
    setDraft(text);
    setEditing(false);
  }

  function save() {
    const next = draft.trim();
    // Guard the no-op cases here (empty / unchanged) so ChatRail never
    // sees a save that would produce a duplicate or empty turn. The
    // textarea pattern is keyboard-only escape; the parent doesn't
    // need to know.
    if (!next || next === text.trim()) {
      cancel();
      return;
    }
    onEdit?.(next);
    setEditing(false);
  }

  return (
    <div data-testid="msg-user" className="group">
      <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-1">
        You{time ? ` · ${time}` : ""}
      </div>
      {editing ? (
        <div className="border-l-2 border-tomato pl-3.5">
          <textarea
            ref={textareaRef}
            data-testid="msg-edit-textarea"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                cancel();
              } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                save();
              }
            }}
            rows={Math.min(10, Math.max(2, draft.split("\n").length))}
            className={cn(
              "w-full resize-none bg-paper-2 border border-rule rounded",
              "px-3 py-2 font-serif text-[15px] leading-snug text-ink",
              "focus:outline-none focus:border-ink",
            )}
          />
          <div className="mt-2 flex items-center gap-2">
            <Button
              type="button"
              variant="primary"
              size="sm"
              data-testid="msg-edit-save"
              onClick={save}
              disabled={!draft.trim() || draft.trim() === text.trim()}
            >
              Save & resend
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              data-testid="msg-edit-cancel"
              onClick={cancel}
            >
              Cancel
            </Button>
          </div>
        </div>
      ) : (
        <>
          <div className="font-serif text-[15px] leading-snug text-ink border-l-2 border-tomato pl-3.5 whitespace-pre-wrap break-words">
            {text}
          </div>
          {attachments && <div className="mt-1 pl-3.5">{attachments}</div>}
          <MessageActions
            copyText={text}
            onEdit={onEdit ? startEdit : undefined}
          />
        </>
      )}
    </div>
  );
}
