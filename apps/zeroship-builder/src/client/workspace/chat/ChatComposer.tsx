import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { Button, Cluster } from "@zeroship/ui";
import { MentionDropdown, type MentionItem } from "./MentionDropdown";
import "./ChatComposer.css";

export interface ChatComposerProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string, attachments: File[]) => void;
  onStop?: () => void;
  busy: boolean;
  disabled?: boolean;
  placeholder?: string;
  /** App id for the @-mention dropdown's RPC fetches. Optional —
   *  without it the dropdown silently shows "no matches". */
  appId?: string;
}

/**
 * Detect an active `@`-mention trigger in the composer text.
 *
 * Returns the byte offset of the `@` and the query text after it (no
 * `@`), or null when the cursor isn't inside a live trigger. A trigger
 * is "live" when:
 *   - the `@` is at position 0 OR preceded by whitespace, AND
 *   - the cursor is somewhere between the `@` and the next whitespace.
 *
 * Two whitespace tokens after the `@` close the trigger (`@file foo`
 * stays open while typing the path; `@file foo bar` closes once the
 * second space is typed). Without this rule, every space in a long
 * message would re-open the dropdown.
 */
function findActiveMention(
  text: string,
  caret: number,
): { start: number; query: string } | null {
  // Walk backwards from the caret to find the most recent `@` that
  // could still be active. Bail early on a newline — the dropdown
  // is single-line.
  let i = caret - 1;
  let spaceCount = 0;
  while (i >= 0) {
    const ch = text[i];
    if (ch === "\n") return null;
    if (ch === " " || ch === "\t") {
      spaceCount++;
      // Keep scanning past at most one space so `@file foo` works
      // while typing the path. After two spaces (or a tab) we stop —
      // the trigger is closed.
      if (spaceCount > 1) return null;
    }
    if (ch === "@") {
      const before = i > 0 ? text[i - 1] : "";
      if (i === 0 || /\s/.test(before)) {
        return { start: i, query: text.slice(i + 1, caret) };
      }
      return null;
    }
    i--;
  }
  return null;
}

export function ChatComposer({
  value,
  onChange,
  onSubmit,
  onStop,
  busy,
  disabled,
  placeholder = "describe a change…",
  appId,
}: ChatComposerProps) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const [attachments, setAttachments] = useState<File[]>([]);

  // @-mention state. `mention` is null when the trigger isn't active.
  const [mention, setMention] = useState<{ start: number; query: string } | null>(
    null,
  );
  const [mentionItems, setMentionItems] = useState<MentionItem[]>([]);
  const [mentionIndex, setMentionIndex] = useState(0);
  // When the user dismisses the dropdown explicitly (Escape), we
  // record the @-position so subsequent recomputes ignore *that* one
  // until the caret moves past it. Otherwise the very next keyUp
  // would re-detect the same `@` and reopen the popover.
  const dismissedAtRef = useRef<number | null>(null);

  /** Recompute the mention trigger from the current cursor. Called on
   *  every change / keyup / select so the dropdown opens or closes
   *  exactly in sync with the caret. */
  const recomputeMention = useCallback(() => {
    const ta = textareaRef.current;
    if (!ta) return;
    const caret = ta.selectionStart ?? value.length;
    const next = findActiveMention(value, caret);
    // If the user dismissed the popover for this @-position, keep it
    // closed until the trigger moves (different start) or the @ is
    // deleted entirely.
    if (next && dismissedAtRef.current === next.start) {
      setMention(null);
      return;
    }
    if (!next || next.start !== dismissedAtRef.current) {
      // Caret has moved away from the dismissed @; clear the lockout.
      dismissedAtRef.current = null;
    }
    setMention((prev) => {
      // Avoid identity churn when the trigger is unchanged so the
      // dropdown's effect doesn't re-fetch on every keystroke that
      // didn't move the trigger.
      if (!prev && !next) return prev;
      if (prev && next && prev.start === next.start && prev.query === next.query) {
        return prev;
      }
      return next;
    });
  }, [value]);

  useEffect(() => {
    recomputeMention();
  }, [recomputeMention]);

  // Reset the active item whenever the suggestion list changes.
  const onMentionItemsChange = useCallback((items: MentionItem[]) => {
    setMentionItems(items);
    setMentionIndex((i) => (i >= items.length ? 0 : i));
  }, []);

  function commitMention(item: MentionItem) {
    if (!mention) return;
    const ta = textareaRef.current;
    const caret = ta?.selectionStart ?? value.length;
    const before = value.slice(0, mention.start);
    const after = value.slice(caret);
    // Always trail with a space so the user can keep typing without
    // immediately re-triggering the dropdown.
    const inserted = `${item.insert} `;
    const next = before + inserted + after;
    onChange(next);
    dismissedAtRef.current = null;
    setMention(null);
    setMentionItems([]);
    setMentionIndex(0);
    // Restore focus + place caret right after the inserted token.
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      const pos = before.length + inserted.length;
      el.setSelectionRange(pos, pos);
    });
  }

  function submit(e?: FormEvent) {
    e?.preventDefault();
    if (busy || disabled) return;
    const text = value.trim();
    if (!text && attachments.length === 0) return;
    onSubmit(text, attachments);
    onChange("");
    setAttachments([]);
    setMention(null);
  }

  function onKey(e: KeyboardEvent<HTMLTextAreaElement>) {
    // Mention navigation takes precedence over send/newline.
    if (mention && mentionItems.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setMentionIndex((i) => (i + 1) % mentionItems.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setMentionIndex((i) => (i - 1 + mentionItems.length) % mentionItems.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const item = mentionItems[mentionIndex];
        if (item) commitMention(item);
        return;
      }
    }
    if (mention && e.key === "Escape") {
      e.preventDefault();
      dismissedAtRef.current = mention.start;
      setMention(null);
      return;
    }
    // ⌘/Ctrl+Enter sends; plain Enter is newline.
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submit();
    }
  }

  function onPickFiles(e: ChangeEvent<HTMLInputElement>) {
    const files = e.target.files;
    if (!files) return;
    setAttachments((prev) => [...prev, ...Array.from(files)]);
    if (fileInputRef.current) fileInputRef.current.value = "";
  }

  function removeAttachment(idx: number) {
    setAttachments((prev) => prev.filter((_, i) => i !== idx));
  }

  return (
    <form onSubmit={submit} data-testid="chat-composer" className="chat-composer">
      {attachments.length > 0 && (
        <Cluster gap={2} className="chat-composer__attachments">
          {attachments.map((f, i) => (
            <span key={i} className="chat-composer__chip">
              <span className="chat-composer__chip-name">{f.name}</span>
              <button
                type="button"
                aria-label={`Remove ${f.name}`}
                onClick={() => removeAttachment(i)}
                className="chat-composer__chip-remove"
              >
                ✕
              </button>
            </span>
          ))}
        </Cluster>
      )}

      {mention && (
        <MentionDropdown
          appId={appId}
          query={mention.query}
          activeIndex={mentionIndex}
          onActiveIndexChange={setMentionIndex}
          onSelect={commitMention}
          onItemsChange={onMentionItemsChange}
        />
      )}

      <textarea
        ref={textareaRef}
        rows={2}
        disabled={busy || disabled}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyUp={recomputeMention}
        onClick={recomputeMention}
        onSelect={recomputeMention}
        onBlur={() => setMention(null)}
        onKeyDown={onKey}
        placeholder={busy ? "thinking…" : placeholder}
        data-testid="chat-input"
        className="chat-composer__textarea"
      />

      <Cluster justify="between" className="chat-composer__footer">
        <span className="chat-composer__hint">
          ⌘ ↵ to send · ↵ for newline · @ to mention
        </span>
        <Cluster gap={2}>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            accept="image/*,.pdf,.json,.txt,.md"
            onChange={onPickFiles}
            className="chat-composer__file-input"
          />
          <Button
            type="button"
            variant="plain"
            size="small"
            onClick={() => fileInputRef.current?.click()}
            aria-label="Attach files"
          >
            📎
          </Button>
          {busy ? (
            <Button
              type="button"
              variant="filled"
              intent="destructive"
              size="small"
              data-testid="chat-stop"
              onClick={onStop}
            >
              ■ Stop
            </Button>
          ) : (
            <Button
              type="submit"
              variant="filled"
              size="small"
              data-testid="chat-send"
              disabled={!value.trim() && attachments.length === 0}
            >
              Send →
            </Button>
          )}
        </Cluster>
      </Cluster>
    </form>
  );
}
