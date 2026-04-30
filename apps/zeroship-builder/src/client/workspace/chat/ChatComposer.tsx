import {
  useRef,
  useState,
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { Button } from "../../components/Button";
import { cn } from "../../lib/utils";

export interface ChatComposerProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string, attachments: File[]) => void;
  onStop?: () => void;
  busy: boolean;
  disabled?: boolean;
  placeholder?: string;
}

export function ChatComposer({
  value,
  onChange,
  onSubmit,
  onStop,
  busy,
  disabled,
  placeholder = "describe a change…",
}: ChatComposerProps) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [attachments, setAttachments] = useState<File[]>([]);

  function submit(e?: FormEvent) {
    e?.preventDefault();
    if (busy || disabled) return;
    const text = value.trim();
    if (!text && attachments.length === 0) return;
    onSubmit(text, attachments);
    onChange("");
    setAttachments([]);
  }

  function onKey(e: KeyboardEvent<HTMLTextAreaElement>) {
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
    <form
      onSubmit={submit}
      data-testid="chat-composer"
      className={cn(
        "border-t border-rule bg-paper",
        "px-4 pt-3 pb-3",
      )}
    >
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-2 mb-2">
          {attachments.map((f, i) => (
            <div
              key={i}
              className="inline-flex items-center gap-1 px-2 py-1 bg-paper-2 border border-rule rounded text-[11px]"
            >
              <span className="font-mono">{f.name}</span>
              <button
                type="button"
                aria-label={`Remove ${f.name}`}
                onClick={() => removeAttachment(i)}
                className="text-ink-soft hover:text-blood cursor-pointer"
              >
                ✕
              </button>
            </div>
          ))}
        </div>
      )}

      <textarea
        rows={2}
        disabled={busy || disabled}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={onKey}
        placeholder={busy ? "thinking…" : placeholder}
        data-testid="chat-input"
        className={cn(
          "w-full resize-none bg-paper-2 border border-rule rounded",
          "px-3 py-2.5 font-serif text-[14.5px] leading-snug text-ink",
          "placeholder:text-pencil placeholder:italic",
          "focus:outline-none focus:border-ink",
          "disabled:opacity-50",
        )}
      />

      <div className="flex items-center justify-between mt-2">
        <div className="flex items-center gap-2 text-[10.5px] text-pencil font-sans uppercase tracking-wider">
          <span>⌘ ↵ to send · ↵ for newline</span>
        </div>
        <div className="flex items-center gap-2">
          <input
            ref={fileInputRef}
            type="file"
            multiple
            accept="image/*,.pdf,.json,.txt,.md"
            onChange={onPickFiles}
            className="hidden"
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => fileInputRef.current?.click()}
            aria-label="Attach files"
          >
            📎
          </Button>
          {busy ? (
            <Button
              type="button"
              variant="destructive"
              size="sm"
              data-testid="chat-stop"
              onClick={onStop}
            >
              ■ Stop
            </Button>
          ) : (
            <Button
              type="submit"
              variant="primary"
              size="sm"
              data-testid="chat-send"
              disabled={!value.trim() && attachments.length === 0}
            >
              Send →
            </Button>
          )}
        </div>
      </div>
    </form>
  );
}
