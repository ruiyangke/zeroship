import { useState } from "react";
import { Spinner } from "../../components/Spinner";

export interface ReceiptProps {
  toolName: string;
  status: "running" | "done" | "error";
  summary?: string;
  inputJson?: unknown;
  outputJson?: unknown;
}

export function Receipt({ toolName, status, summary, inputJson, outputJson }: ReceiptProps) {
  const [open, setOpen] = useState(false);
  const showDetails = inputJson !== undefined || outputJson !== undefined;

  return (
    <div
      data-testid="receipt"
      data-status={status}
      className="mt-2 bg-paper border border-rule-2 rounded px-3 py-2.5"
    >
      <div className="flex items-start gap-3">
        <span aria-hidden="true" className="flex h-[18px] w-[18px] flex-shrink-0 items-center justify-center mt-0.5">
          {status === "done" && (
            <span className="inline-block size-[14px] rounded-full bg-ivy text-paper text-[10px] font-semibold leading-none flex items-center justify-center">
              ✓
            </span>
          )}
          {status === "error" && (
            <span className="inline-block size-[14px] rounded-full bg-blood text-paper text-[10px] font-semibold leading-none flex items-center justify-center">
              ✕
            </span>
          )}
          {status === "running" && <Spinner size={14} />}
        </span>
        <div className="flex-1 min-w-0 font-serif text-[13.5px] leading-snug text-ink">
          {summary ?? toolName}
          <div className="font-mono text-[10px] text-pencil mt-0.5">{toolName}</div>
        </div>
        {showDetails && (
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="self-center font-sans text-[11px] text-pencil hover:text-ink cursor-pointer"
          >
            {open ? "hide" : "details"}
          </button>
        )}
      </div>
      {open && (
        <div className="mt-2 pt-2 border-t border-rule-2 space-y-2">
          {inputJson !== undefined && (
            <div>
              <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-0.5">input</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-32 overflow-auto bg-paper-2 px-2 py-1 border border-rule-2 rounded">
                {JSON.stringify(inputJson, null, 2)}
              </pre>
            </div>
          )}
          {outputJson !== undefined && (
            <div>
              <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-0.5">output</div>
              <pre className="font-mono text-[10.5px] whitespace-pre-wrap break-all max-h-32 overflow-auto bg-paper-2 px-2 py-1 border border-rule-2 rounded">
                {typeof outputJson === "string" ? outputJson : JSON.stringify(outputJson, null, 2)}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
