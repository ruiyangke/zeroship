// ─── useBuilderChat ─────────────────────────────────────────────
//
// Owns chat state for one app: message history, in-flight assistant
// turn, tool-call accumulation, status flag. Persists to
// localStorage on every mutation. The deploy callback is fired
// when a `deploy_app`-shaped tool finishes successfully — the
// preview pane uses it to reload the iframe.

import { useCallback, useEffect, useRef, useState } from "react";
import type { AgentEvent, BuilderStatus, ChatMessage, ToolEvent } from "./types";
import { genId, streamAgent, type AgentContext } from "./agentClient";
import { loadConversation, saveConversation } from "./storage";

const DEPLOY_TOOLS = new Set(["deploy_app", "deploy_full_app", "build_and_publish"]);
/** Tools whose successful completion should trigger iframe reload. */
const APP_MUTATION_TOOLS = new Set(["deploy_app", "create_app", "build_and_publish"]);
/** Tools whose result may carry a freshly-provisioned app_id we should navigate to. */
const APP_PROVISION_TOOLS = new Set(["create_app", "build_and_publish"]);

interface Options {
  /** Stable id per project; used as thread_id and storage key. */
  appId: string;
  /** Workspace context fed to the agent on every turn. */
  context?: AgentContext;
  /** Called after a successful deploy — for iframe reloads. */
  onDeploy?: () => void;
  /** Called after create_app succeeds, with the new app_id. */
  onAppCreated?: (appId: string, appName: string) => void;
}

interface ChatActions {
  send: (text: string) => Promise<void>;
  cancel: () => void;
  reset: () => void;
}

export interface BuilderChat {
  messages: ChatMessage[];
  status: BuilderStatus;
  error: string | null;
  actions: ChatActions;
}

export function useBuilderChat({ appId, context, onDeploy, onAppCreated }: Options): BuilderChat {
  const [messages, setMessages] = useState<ChatMessage[]>(() => loadConversation(appId));
  const [status, setStatus] = useState<BuilderStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const onDeployRef = useRef(onDeploy);
  const onAppCreatedRef = useRef(onAppCreated);
  onDeployRef.current = onDeploy;
  onAppCreatedRef.current = onAppCreated;

  // Re-load when the page switches between projects.
  useEffect(() => {
    setMessages(loadConversation(appId));
    setStatus("idle");
    setError(null);
    if (abortRef.current) {
      abortRef.current.abort();
      abortRef.current = null;
    }
  }, [appId]);

  // Persist on every change. Cheap because we never hold large blobs.
  useEffect(() => {
    saveConversation(appId, messages);
  }, [appId, messages]);

  const cancel = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
    setStatus("idle");
  }, []);

  const reset = useCallback(() => {
    cancel();
    setMessages([]);
    setError(null);
  }, [cancel]);

  const send = useCallback(
    async (text: string) => {
      const trimmed = text.trim();
      if (!trimmed || status !== "idle") return;

      const userMsg: ChatMessage = {
        id: genId(),
        role: "user",
        content: trimmed,
        createdAt: Date.now(),
      };

      const assistantMsg: ChatMessage = {
        id: genId(),
        role: "assistant",
        content: "",
        tools: [],
        createdAt: Date.now(),
      };

      // Snapshot of what we'll send to the agent (before the assistant placeholder).
      const baseMessages = [...messages, userMsg];
      setMessages([...baseMessages, assistantMsg]);
      setStatus("thinking");
      setError(null);

      const controller = new AbortController();
      abortRef.current = controller;
      let didDeploySucceed = false;

      const updateAssistant = (mut: (m: ChatMessage) => ChatMessage) => {
        setMessages((prev) => {
          const out = prev.slice();
          const idx = out.findIndex((m) => m.id === assistantMsg.id);
          if (idx < 0) return prev;
          out[idx] = mut(out[idx]);
          return out;
        });
      };

      const onEvent = (ev: AgentEvent) => {
        switch (ev.type) {
          case "text":
            updateAssistant((m) => ({ ...m, content: m.content + ev.content }));
            setStatus("thinking");
            break;
          case "tool_start": {
            const tool: ToolEvent = {
              id: genId(),
              name: ev.name,
              input: ev.input,
              done: false,
            };
            updateAssistant((m) => ({ ...m, tools: [...(m.tools ?? []), tool] }));
            setStatus(DEPLOY_TOOLS.has(ev.name) ? "deploying" : "calling-tool");
            break;
          }
          case "tool_end": {
            const out = typeof ev.output === "string" ? ev.output : JSON.stringify(ev.output);
            const isErr = ev.error === true;

            // Sniff the tool result for app provisioning side-effects.
            // Both create_app and deploy_app return a JSON string with
            // a known shape; we parse defensively.
            let parsed: any = null;
            try { parsed = JSON.parse(out); } catch { /* not JSON */ }

            // create_app and build_and_publish both surface app_id +
            // app_name on success — when we're still in bootstrap
            // mode (synthetic appId), let the page navigate to the
            // freshly-provisioned project so subsequent tool calls
            // operate against the right URL.
            if (!isErr && APP_PROVISION_TOOLS.has(ev.name) && parsed?.ok && parsed?.app_id) {
              onAppCreatedRef.current?.(
                String(parsed.app_id),
                String(parsed.app_name ?? ""),
              );
            }
            if (!isErr && APP_MUTATION_TOOLS.has(ev.name)) {
              didDeploySucceed = true;
            }

            updateAssistant((m) => {
              const tools = (m.tools ?? []).slice();
              for (let i = tools.length - 1; i >= 0; i--) {
                if (tools[i].name === ev.name && !tools[i].done) {
                  tools[i] = {
                    ...tools[i],
                    done: true,
                    output: out,
                    error: isErr,
                  };
                  break;
                }
              }
              return { ...m, tools };
            });
            setStatus("thinking");
            break;
          }
          case "done":
            // Finalized in the await below.
            break;
          case "error":
            setError(ev.content);
            updateAssistant((m) => ({
              ...m,
              content: m.content + (m.content ? "\n\n" : "") + `**Error:** ${ev.content}`,
            }));
            break;
        }
      };

      try {
        await streamAgent(baseMessages, appId, {
          signal: controller.signal,
          context,
          onEvent,
        });
        if (didDeploySucceed) onDeployRef.current?.();
      } catch (err) {
        if ((err as Error).name === "AbortError") {
          // Cancellation is user-driven; keep the partial assistant message.
        } else {
          const msg = (err as Error).message ?? String(err);
          setError(msg);
          updateAssistant((m) => ({
            ...m,
            content: m.content + (m.content ? "\n\n" : "") + `**Error:** ${msg}`,
          }));
        }
      } finally {
        abortRef.current = null;
        setStatus("idle");
      }
    },
    [appId, context, messages, status],
  );

  return {
    messages,
    status,
    error,
    actions: { send, cancel, reset },
  };
}
