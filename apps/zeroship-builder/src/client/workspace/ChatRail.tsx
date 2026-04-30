// ─── ChatRail — desk-side notebook ──────────────────────────────
//
// User turns: tomato accent line + serif body
// Assistant turns: small-caps "THE STUDIO" + serif body, with
//                  paper Receipts for tool calls
// Composer: notebook pad with red ruler, italic placeholder, tomato Send

import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import type { BuilderChat } from "../builder/useBuilderChat";
import type { ChatMessage } from "../builder/types";
import { Receipt } from "../components/Receipt";

export interface ChatRailProps {
  chat: BuilderChat;
  appName?: string;
}

export function ChatRail({ chat, appName }: ChatRailProps) {
  const { messages, status, error, actions } = chat;
  const [draft, setDraft] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, status]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = dist < 60;
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

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-baseline justify-between px-6 pt-5 pb-3 border-b border-dashed border-rule">
        <h3 className="font-serif italic font-medium text-[18px] m-0">Notes &amp; thoughts</h3>
        <div className="flex items-center gap-3">
          <span className="font-sans text-[10px] uppercase tracking-[0.18em] text-pencil">
            {messages.length} {messages.length === 1 ? "turn" : "turns"}
          </span>
          {messages.length > 0 && (
            <button
              type="button"
              onClick={actions.reset}
              title="Clear conversation"
              className="font-serif italic text-[11px] text-pencil hover:text-tomato bg-transparent border-0 cursor-pointer"
              data-testid="chat-clear"
            >
              clear
            </button>
          )}
        </div>
      </div>

      <div ref={scrollRef} onScroll={onScroll} className="flex-1 overflow-y-auto px-6 py-5 flex flex-col gap-6">
        {messages.length === 0 ? (
          <EmptyState />
        ) : (
          messages.map((m, i) => (
            <Turn
              key={m.id}
              message={m}
              streaming={busy && i === messages.length - 1 && m.role === "assistant"}
            />
          ))
        )}
      </div>

      {error && (
        <div className="px-6 py-3 border-t border-tomato bg-tomato/10 font-serif italic text-tomato text-[13px]">
          {error}
        </div>
      )}

      <form onSubmit={submit} className="border-t border-rule bg-paper px-5 py-4">
        <div
          className="relative bg-white border border-rule px-4 py-3"
          style={{ borderLeftWidth: "1px" }}
        >
          <span
            aria-hidden="true"
            className="absolute top-0 bottom-0 w-px"
            style={{ left: "8px", background: "var(--color-tomato)", opacity: 0.3 }}
          />
          <textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKey}
            disabled={busy}
            rows={2}
            placeholder={
              busy
                ? "the studio is working…"
                : messages.length === 0
                  ? `Tell ${appName ?? "the studio"} what to make.`
                  : "Iterate, refine, or ask for something new…"
            }
            className="w-full resize-none border-0 bg-transparent text-ink outline-none font-serif text-[14.5px] leading-[1.5] placeholder:text-pencil placeholder:italic min-h-[40px]"
            data-testid="chat-input"
          />
          <div className="flex items-center justify-between mt-2">
            <span className="font-sans text-[10.5px] uppercase tracking-[0.16em] text-pencil">
              ↵ to send · ⇧↵ for newline
            </span>
            {busy ? (
              <button
                type="button"
                onClick={actions.cancel}
                data-testid="chat-stop"
                className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-tomato bg-transparent border border-tomato px-3 py-1.5 hover:bg-tomato hover:text-paper transition-colors cursor-pointer"
              >
                ■ Stop
              </button>
            ) : (
              <button
                type="submit"
                disabled={!draft.trim()}
                data-testid="chat-send"
                className="bg-tomato text-paper border-0 px-3.5 py-1.5 font-sans font-semibold text-[10.5px] uppercase tracking-[0.18em] cursor-pointer disabled:opacity-50 hover:translate-y-[-1px] transition-transform"
                style={{ boxShadow: "0 2px 0 -1px var(--color-tomato-2)" }}
              >
                Send <span className="font-serif italic font-normal">→</span>
              </button>
            )}
          </div>
        </div>
      </form>
    </div>
  );
}

function Turn({ message: m, streaming }: { message: ChatMessage; streaming?: boolean }) {
  if (m.role === "user") {
    return (
      <div data-testid="turn-user">
        <div className="font-serif italic text-[12px] text-tomato font-medium mb-1.5">
          You · {fmtTime(m.createdAt)}
        </div>
        <div className="font-serif text-[15px] leading-[1.45] text-ink border-l-2 border-tomato pl-3.5">
          {m.content}
        </div>
      </div>
    );
  }
  return (
    <div data-testid="turn-bot">
      <div className="font-sans text-[10px] uppercase tracking-[0.2em] text-ink-soft mb-1.5">
        The studio
      </div>
      <div className="font-serif text-[14.5px] leading-[1.55] text-ink whitespace-pre-wrap break-words">
        {m.content}
        {streaming && (
          <span className="inline-block ml-0.5 w-[6px] h-[14px] bg-ink/70 align-middle" style={{ animation: "pulse 1s ease-in-out infinite" }} />
        )}
        {!m.content && streaming && (
          <span className="italic text-pencil">thinking…</span>
        )}
      </div>
      {m.tools && m.tools.length > 0 && (
        <div className="mt-1">
          {m.tools.map((t) => <Receipt key={t.id} tool={t} />)}
        </div>
      )}
    </div>
  );
}

function EmptyState() {
  return (
    <div className="h-full flex items-start pt-12">
      <div>
        <p className="font-serif italic text-[15px] text-ink-soft leading-[1.55] mb-3">
          Tell the studio what to make. The agent will draft, build, and ship it as a real URL.
        </p>
        <ul className="font-serif italic text-[13.5px] text-pencil list-none p-0 m-0 leading-[1.9]">
          <li>"build me a recipe sharing app"</li>
          <li>"a tip calculator with split-by-people"</li>
          <li>"a markdown notes editor with localStorage"</li>
        </ul>
      </div>
    </div>
  );
}

function fmtTime(ms: number): string {
  const d = new Date(ms);
  const h = String(d.getHours()).padStart(2, "0");
  const m = String(d.getMinutes()).padStart(2, "0");
  return `${h}:${m}`;
}
