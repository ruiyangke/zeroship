// ─── Receipt — confirmation receipt for tool calls ──────────────
//
// Replaces the JSON-dump tool-call card. Renders one of:
//   - running:  small spinner + plain-language description
//   - done:     success check + completed action sentence
//   - error:    danger ✕ + a clear "what went wrong"
//
// Translation from the technical tool name + input/output to a
// plain-language sentence happens in `humanize()` below; the chat
// rail passes the raw ToolEvent and we figure out what to say.
//
// Rebuilt on @zeroship/ui crystal: a Card surface, a Spinner/Icon
// status indicator, a Tag caption, a Collapsible details disclosure,
// and a DescriptionList for the input/output key→value pairs.

import type { ReactNode } from "react";
import {
  Card,
  Collapsible,
  DescriptionList,
  Spinner,
  Stack,
  Tag,
} from "@zeroship/ui";
import type { ToolEvent } from "../builder/types";
import "./Receipt.css";

export interface ReceiptProps {
  tool: ToolEvent;
}

export function Receipt({ tool }: ReceiptProps) {
  const status: "run" | "done" | "error" = tool.error
    ? "error"
    : tool.done
      ? "done"
      : "run";
  const sentence = humanize(tool);
  const hasDetails = tool.input !== undefined || tool.output !== undefined;

  return (
    <Card variant="outline" size="sm" className="zs-receipt" data-status={status}>
      <Card.Content>
        <Stack direction="row" gap={2} align="start" className="zs-receipt__row">
          <span className="zs-receipt__status" aria-hidden="true">
            {status === "run" ? (
              <Spinner size="sm" label="Working" />
            ) : (
              <span className="zs-receipt__glyph">
                {status === "done" ? "✓" : "✕"}
              </span>
            )}
          </span>

          <Stack gap={1} className="zs-receipt__body">
            <span className="zs-receipt__sentence">{sentence}</span>
            <Tag size="sm" className="zs-receipt__caption">
              {status === "run" ? "in progress" : tool.name}
            </Tag>
          </Stack>

          {hasDetails && (
            <Collapsible className="zs-receipt__disclosure">
              <Collapsible.Trigger className="zs-receipt__toggle">
                details
              </Collapsible.Trigger>
              <Collapsible.Panel>
                <DescriptionList
                  orientation="vertical"
                  divider
                  className="zs-receipt__details"
                >
                  {tool.input !== undefined && (
                    <DescriptionList.Item>
                      <DescriptionList.Term>input</DescriptionList.Term>
                      <DescriptionList.Detail>
                        <pre className="zs-receipt__pre zs-receipt__pre--input">
                          {safeStringify(tool.input)}
                        </pre>
                      </DescriptionList.Detail>
                    </DescriptionList.Item>
                  )}
                  {tool.output !== undefined && (
                    <DescriptionList.Item>
                      <DescriptionList.Term>output</DescriptionList.Term>
                      <DescriptionList.Detail>
                        <pre className="zs-receipt__pre zs-receipt__pre--output">
                          {tool.output.length > 1200
                            ? tool.output.slice(0, 1200) + "\n…"
                            : tool.output}
                        </pre>
                      </DescriptionList.Detail>
                    </DescriptionList.Item>
                  )}
                </DescriptionList>
              </Collapsible.Panel>
            </Collapsible>
          )}
        </Stack>
      </Card.Content>
    </Card>
  );
}

/** Convert a tool name + payload into a human sentence. */
function humanize(t: ToolEvent): ReactNode {
  const name = t.name;
  const inp: any = t.input ?? {};

  // Sandbox file ops
  if (name === "sandbox_write_file" && inp.path) {
    return (
      <>
        Wrote <code className="zs-receipt__code">{String(inp.path)}</code>.
      </>
    );
  }
  if (name === "sandbox_read_file" && inp.path) {
    return (
      <>
        Read <code className="zs-receipt__code">{String(inp.path)}</code>.
      </>
    );
  }
  if (name === "sandbox_delete_file" && inp.path) {
    return (
      <>
        Deleted <code className="zs-receipt__code">{String(inp.path)}</code>.
      </>
    );
  }
  if (name === "sandbox_list_files") {
    return <>Looked at the project files.</>;
  }
  if (name === "sandbox_exec" && inp.cmd) {
    return (
      <>
        Ran{" "}
        <code className="zs-receipt__code">{String(inp.cmd).slice(0, 60)}</code>
        {String(inp.cmd).length > 60 ? "…" : ""} in the project.
      </>
    );
  }
  if (name === "open_session") {
    return <>Opened the project's sandbox.</>;
  }

  // Apps / control plane
  if (name === "list_apps") return <>Looked up the platform's apps.</>;
  if (name === "create_app" && inp.name)
    return (
      <>
        Created an app called <em>{String(inp.name)}</em>.
      </>
    );
  if (
    name === "deploy_app" ||
    name === "deploy_full_app" ||
    name === "build_and_publish"
  ) {
    return (
      <>
        Built and shipped the app — <em>it's live in a moment</em>.
      </>
    );
  }

  // Fallback: show the tool name in italic
  return (
    <>
      Ran <em>{name}</em>.
    </>
  );
}

function safeStringify(v: unknown): string {
  if (typeof v === "string") return v;
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}
