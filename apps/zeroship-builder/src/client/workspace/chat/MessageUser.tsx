import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button, Stack } from "@zeroship/ui";
import { MessageActions } from "./MessageActions";
import "./MessageUser.css";

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

  // `group` stays on the root: MessageActions reveals its hover row via
  // the parent group-hover state. Crystal presentation lives in the
  // co-located CSS keyed off `.msg-user`.
  return (
    <Stack data-testid="msg-user" className="group msg-user" gap={1}>
      <div className="msg-user__label">
        You{time ? ` · ${time}` : ""}
      </div>
      {editing ? (
        <div className="msg-user__edit">
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
            className="msg-user__textarea"
          />
          <Stack
            direction="row"
            gap={2}
            align="center"
            className="msg-user__edit-actions"
          >
            <Button
              type="button"
              variant="filled"
              size="small"
              data-testid="msg-edit-save"
              onClick={save}
              disabled={!draft.trim() || draft.trim() === text.trim()}
            >
              Save & resend
            </Button>
            <Button
              type="button"
              variant="plain"
              size="small"
              data-testid="msg-edit-cancel"
              onClick={cancel}
            >
              Cancel
            </Button>
          </Stack>
        </div>
      ) : (
        <>
          <div className="msg-user__body">{text}</div>
          {attachments && (
            <div className="msg-user__attachments">{attachments}</div>
          )}
          <MessageActions
            copyText={text}
            onEdit={onEdit ? startEdit : undefined}
          />
        </>
      )}
    </Stack>
  );
}
