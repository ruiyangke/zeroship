// ─── Chat ───────────────────────────────────────────────────────
// Scrollable message list + sticky composer at the bottom.
// Auto-scrolls to bottom on new content unless the user has scrolled
// up (a common UX bug we explicitly avoid).

import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import { Send, Loader2, Square } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import type { BuilderChat } from "../useBuilderChat";
import { Message } from "./Message";

interface Props {
  chat: BuilderChat;
}

export function Chat({ chat }: Props) {
  const { messages, status, error, actions } = chat;
  const [draft, setDraft] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [messages, status]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = distance < 60;
  }

  function submit(e?: FormEvent) {
    e?.preventDefault();
    if (!draft.trim() || status !== "idle") return;
    const text = draft;
    setDraft("");
    stickToBottom.current = true;
    void actions.send(text);
  }

  function onKey(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }

  const busy = status !== "idle";
  const isStreamingAssistant = busy && messages[messages.length - 1]?.role === "assistant";

  return (
    <div className="flex flex-col h-full bg-background">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="flex-1 overflow-y-auto"
      >
        {messages.length === 0 ? (
          <EmptyState />
        ) : (
          messages.map((m, i) => (
            <Message
              key={m.id}
              message={m}
              streaming={isStreamingAssistant && i === messages.length - 1}
            />
          ))
        )}
      </div>

      {error && (
        <div className="px-3 py-2 text-xs text-destructive border-t border-destructive/40 bg-destructive/10">
          {error}
        </div>
      )}

      <form onSubmit={submit} className="border-t border-border bg-background p-2">
        <div className="flex gap-2 items-end">
          <Textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKey}
            disabled={busy}
            placeholder={
              busy
                ? statusLabel(status)
                : messages.length === 0
                  ? "Describe what you want to build…"
                  : "Iterate, refine, or ask a question…"
            }
            rows={2}
            className="resize-none min-h-[60px] text-sm font-mono"
          />
          {busy ? (
            <Button
              type="button"
              variant="outline"
              onClick={actions.cancel}
              title="Stop generation"
              className="h-[60px] px-3"
            >
              <Square className="size-4 fill-current" />
            </Button>
          ) : (
            <Button
              type="submit"
              variant="primary"
              disabled={!draft.trim()}
              className="h-[60px] px-3"
              title="Send (Enter)"
            >
              <Send className="size-4" />
            </Button>
          )}
        </div>
        <div className="text-[10px] text-muted-foreground/70 mt-1 px-0.5 flex items-center gap-1.5">
          {busy && <Loader2 className="size-2.5 animate-spin" />}
          <span>{busy ? statusLabel(status) : "Enter to send · Shift+Enter for newline"}</span>
        </div>
      </form>
    </div>
  );
}

function EmptyState() {
  return (
    <div className="h-full flex items-center justify-center px-6">
      <div className="text-center max-w-sm">
        <div className="text-xs uppercase tracking-[0.2em] text-muted-foreground mb-3">
          // ai builder
        </div>
        <p className="text-sm text-foreground/80 leading-relaxed">
          Describe the app you want to build and the agent will generate it,
          deploy it, and surface the live preview on the right.
        </p>
        <div className="mt-6 grid gap-1.5 text-left text-xs font-mono text-muted-foreground">
          <div>· "build me a recipe sharing app"</div>
          <div>· "a tip calculator with split-by-people"</div>
          <div>· "a markdown notes editor with localStorage"</div>
        </div>
      </div>
    </div>
  );
}

function statusLabel(s: string): string {
  switch (s) {
    case "thinking": return "thinking…";
    case "calling-tool": return "running tool…";
    case "deploying": return "deploying…";
    case "error": return "error";
    default: return "";
  }
}
