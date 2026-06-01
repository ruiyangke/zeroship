// Receipt — chat-stream tool-call receipt (distinct from components/Receipt).
//
// A compact card noting a tool invocation: a status glyph (running spinner /
// done check / error cross), the human summary with the raw tool name beneath,
// and an optional details toggle that reveals the input/output JSON.
//
// Crystal: a DS Card (variant="outline") frames the receipt. The expandable
// details use a DS DescriptionList (input → <pre>, output → <pre>) so the
// labels read as semantic terms. The "details"/"hide" toggle is a DS Button
// (variant="plain"); the running state uses the DS Spinner. Bespoke bits — the
// status glyph circles and the JSON code blocks — live in Receipt.css over
// --zs-* tokens. The public props, behaviour, and the `receipt` /
// `data-status` test hooks are preserved exactly.

import { useState } from "react";
import { Button, Card, DescriptionList, Spinner } from "@zeroship/ui";
import "./Receipt.css";

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
    <Card
      variant="outline"
      size="sm"
      data-testid="receipt"
      data-status={status}
      className="receipt"
    >
      <div className="receipt__head">
        <span aria-hidden="true" className="receipt__status">
          {status === "done" && (
            <span className="receipt__glyph receipt__glyph--done">✓</span>
          )}
          {status === "error" && (
            <span className="receipt__glyph receipt__glyph--error">✕</span>
          )}
          {status === "running" && <Spinner size="sm" />}
        </span>
        <div className="receipt__text">
          {summary ?? toolName}
          <div className="receipt__tool">{toolName}</div>
        </div>
        {showDetails && (
          <Button
            type="button"
            variant="plain"
            size="small"
            className="receipt__toggle"
            onClick={() => setOpen((v) => !v)}
          >
            {open ? "hide" : "details"}
          </Button>
        )}
      </div>
      {open && (
        <DescriptionList orientation="vertical" className="receipt__details">
          {inputJson !== undefined && (
            <DescriptionList.Item>
              <DescriptionList.Term>input</DescriptionList.Term>
              <DescriptionList.Detail>
                <pre className="receipt__json">{JSON.stringify(inputJson, null, 2)}</pre>
              </DescriptionList.Detail>
            </DescriptionList.Item>
          )}
          {outputJson !== undefined && (
            <DescriptionList.Item>
              <DescriptionList.Term>output</DescriptionList.Term>
              <DescriptionList.Detail>
                <pre className="receipt__json">
                  {typeof outputJson === "string"
                    ? outputJson
                    : JSON.stringify(outputJson, null, 2)}
                </pre>
              </DescriptionList.Detail>
            </DescriptionList.Item>
          )}
        </DescriptionList>
      )}
    </Card>
  );
}
