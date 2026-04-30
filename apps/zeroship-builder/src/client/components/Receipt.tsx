// ─── Receipt — paper receipt for tool calls ─────────────────────
//
// Replaces the JSON-dump tool-call card. Renders one of:
//   - running:  small spinner + plain-language description
//   - done:     tomato checkmark + completed action sentence
//   - error:    tomato X + a clear "what went wrong"
//
// Translation from the technical tool name + input/output to a
// plain-language sentence happens in `humanize()` below; the chat
// rail passes the raw ToolEvent and we figure out what to say.

import { useState } from "react";
import type { ToolEvent } from "../builder/types";
import { cn } from "../lib/utils";

export interface ReceiptProps {
  tool: ToolEvent;
}

export function Receipt({ tool }: ReceiptProps) {
  const [open, setOpen] = useState(false);
  const status: "run" | "done" | "error" = tool.error ? "error" : tool.done ? "done" : "run";
  const sentence = humanize(tool);

  return (
    <div className="mt-2 bg-white border border-rule-2 px-3 py-2.5 flex items-start gap-3">
      <span
        className={cn(
          "flex h-[18px] w-[18px] flex-shrink-0 items-center justify-center rounded-full font-sans text-[10px] font-semibold leading-none mt-0.5",
          status === "done" && "bg-tomato text-paper",
          status === "error" && "bg-tomato text-paper",
          status === "run" && "bg-paper-3 relative",
        )}
        aria-hidden="true"
      >
        {status === "done" && "✓"}
        {status === "error" && "✕"}
        {status === "run" && (
          <span
            className="absolute inset-[3px] rounded-full border-[1.5px] border-ink-soft border-t-transparent spin"
          />
        )}
      </span>
      <div className="flex-1 min-w-0 font-serif text-[13.5px] leading-snug text-ink">
        {sentence}
        <span className="block font-sans text-[9.5px] uppercase tracking-[0.16em] text-pencil mt-1">
          {status === "run" ? "in progress" : `${tool.name}`}
        </span>
      </div>
      {(tool.input !== undefined || tool.output !== undefined) && (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="self-center font-serif italic text-[12px] text-pencil hover:text-ink bg-transparent border-0 cursor-pointer"
        >
          {open ? "hide" : "details"}
        </button>
      )}
      {open && (
        <div className="absolute left-0 right-0 mt-12 mx-3 px-3 pb-3 pt-2 border-t border-rule-2 bg-white space-y-2 font-mono text-[10.5px]">
          {/* Inline expansion is hard inside a flex row; render below as a sibling section. */}
        </div>
      )}
      {open && (
        <div className="basis-full mt-2 pt-2 border-t border-rule-2 space-y-2 order-last w-full">
          {tool.input !== undefined && (
            <div>
              <div className="label-uc mb-0.5">input</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-32 overflow-auto bg-paper px-2 py-1 border border-rule-2">
                {safeStringify(tool.input)}
              </pre>
            </div>
          )}
          {tool.output !== undefined && (
            <div>
              <div className="label-uc mb-0.5">output</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-48 overflow-auto bg-paper px-2 py-1 border border-rule-2">
                {tool.output.length > 1200 ? tool.output.slice(0, 1200) + "\n…" : tool.output}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Convert a tool name + payload into a human sentence. */
function humanize(t: ToolEvent): React.ReactNode {
  const name = t.name;
  const inp: any = t.input ?? {};

  // Sandbox file ops
  if (name === "sandbox_write_file" && inp.path) {
    return <>Wrote <code className="font-mono text-[12px] bg-paper-2 px-1 rounded-[2px]">{String(inp.path)}</code>.</>;
  }
  if (name === "sandbox_read_file" && inp.path) {
    return <>Read <code className="font-mono text-[12px] bg-paper-2 px-1 rounded-[2px]">{String(inp.path)}</code>.</>;
  }
  if (name === "sandbox_delete_file" && inp.path) {
    return <>Deleted <code className="font-mono text-[12px] bg-paper-2 px-1 rounded-[2px]">{String(inp.path)}</code>.</>;
  }
  if (name === "sandbox_list_files") {
    return <>Looked at the project files.</>;
  }
  if (name === "sandbox_exec" && inp.cmd) {
    return <>Ran <code className="font-mono text-[12px] bg-paper-2 px-1 rounded-[2px]">{String(inp.cmd).slice(0, 60)}</code>{String(inp.cmd).length > 60 ? "…" : ""} in the project.</>;
  }
  if (name === "open_session") {
    return <>Opened the project's sandbox.</>;
  }

  // Apps / control plane
  if (name === "list_apps") return <>Looked up the platform's apps.</>;
  if (name === "create_app" && inp.name) return <>Created an app called <em className="italic">{String(inp.name)}</em>.</>;
  if (name === "deploy_app" || name === "deploy_full_app" || name === "build_and_publish") {
    return <>Built and shipped the app — <em className="italic">it's live in a moment</em>.</>;
  }

  // Fallback: show the tool name in italic
  return <>Ran <em className="italic">{name}</em>.</>;
}

function safeStringify(v: unknown): string {
  if (typeof v === "string") return v;
  try { return JSON.stringify(v, null, 2); } catch { return String(v); }
}
