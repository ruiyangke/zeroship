import { useState, useRef, useEffect } from "react";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";
import { Bot, User, Send, Loader2, Wrench, CheckCircle, AlertCircle } from "lucide-react";

const AGENT_URL = "http://localhost:4444";

interface Message {
  role: "user" | "assistant";
  content: string;
  tools?: ToolEvent[];
}

interface ToolEvent {
  type: "tool_start" | "tool_end";
  name: string;
  input?: any;
  output?: string;
}

export default function AIChat() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [currentTools, setCurrentTools] = useState<ToolEvent[]>([]);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, streaming]);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!input.trim() || streaming) return;

    const userMsg: Message = { role: "user", content: input.trim() };
    const newMessages = [...messages, userMsg];
    setMessages(newMessages);
    setInput("");
    setStreaming(true);
    setCurrentTools([]);

    let assistantContent = "";
    const tools: ToolEvent[] = [];

    try {
      const res = await fetch(`${AGENT_URL}/chat`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          messages: newMessages.map((m) => ({
            role: m.role,
            content: m.content,
          })),
        }),
      });

      if (!res.ok) {
        throw new Error(`Agent returned ${res.status}`);
      }

      const reader = res.body?.getReader();
      if (!reader) throw new Error("No response body");

      const decoder = new TextDecoder();
      let buffer = "";

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() || "";

        for (const line of lines) {
          if (!line.startsWith("data: ")) continue;
          const jsonStr = line.slice(6).trim();
          if (!jsonStr) continue;

          try {
            const event = JSON.parse(jsonStr);

            if (event.type === "text") {
              assistantContent += event.content;
              setMessages([
                ...newMessages,
                { role: "assistant", content: assistantContent, tools },
              ]);
            } else if (event.type === "tool_start") {
              const toolEvt: ToolEvent = {
                type: "tool_start",
                name: event.name,
                input: event.input,
              };
              tools.push(toolEvt);
              setCurrentTools([...tools]);
            } else if (event.type === "tool_end") {
              const toolEvt: ToolEvent = {
                type: "tool_end",
                name: event.name,
                output:
                  typeof event.output === "string"
                    ? event.output
                    : JSON.stringify(event.output),
              };
              tools.push(toolEvt);
              setCurrentTools([...tools]);
            } else if (event.type === "error") {
              assistantContent += `\n\nError: ${event.content}`;
              setMessages([
                ...newMessages,
                { role: "assistant", content: assistantContent, tools },
              ]);
            }
          } catch {
            // Skip malformed JSON
          }
        }
      }

      // Final message
      if (assistantContent || tools.length > 0) {
        setMessages([
          ...newMessages,
          { role: "assistant", content: assistantContent, tools },
        ]);
      }
    } catch (err: any) {
      setMessages([
        ...newMessages,
        {
          role: "assistant",
          content: `Failed to reach AI agent: ${err.message}\n\nMake sure the agent is running:\n\`\`\`\ncd agent && ANTHROPIC_API_KEY=sk-... bun run dev\n\`\`\``,
        },
      ]);
    } finally {
      setStreaming(false);
      setCurrentTools([]);
    }
  }

  function handleKeyDown(e: React.KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSubmit(e);
    }
  }

  return (
    <div className="flex flex-col h-[calc(100vh-4rem)]">
      <div className="flex items-center gap-2 mb-4">
        <Bot className="h-5 w-5 text-primary" />
        <h1 className="text-lg font-bold tracking-tight">AI AGENT</h1>
        <Badge variant="outline" className="text-xs">
          deepagents + claude
        </Badge>
      </div>

      {/* Messages */}
      <div className="flex-1 overflow-y-auto space-y-4 pb-4">
        {messages.length === 0 && (
          <Card className="border-border bg-card">
            <CardContent className="pt-6">
              <p className="text-muted-foreground text-sm mb-4">
                Describe the app you want to build. The AI agent will generate
                the code, deploy it to the platform, and test it.
              </p>
              <div className="grid grid-cols-1 md:grid-cols-2 gap-2">
                {[
                  "Build a todo API with add, list, and delete methods",
                  "Create a URL shortener that stores links in memory",
                  "Build a weather proxy that fetches from wttr.in",
                  "Make a math API with add, multiply, and factorial",
                ].map((example) => (
                  <button
                    key={example}
                    className="text-left text-xs p-3 border border-border hover:border-primary hover:text-primary transition-colors"
                    onClick={() => setInput(example)}
                  >
                    {example}
                  </button>
                ))}
              </div>
            </CardContent>
          </Card>
        )}

        {messages.map((msg, i) => (
          <div key={i} className="flex gap-3">
            <div className="flex-shrink-0 mt-1">
              {msg.role === "user" ? (
                <User className="h-4 w-4 text-muted-foreground" />
              ) : (
                <Bot className="h-4 w-4 text-primary" />
              )}
            </div>
            <div className="flex-1 min-w-0">
              {/* Tool events */}
              {msg.tools && msg.tools.length > 0 && (
                <div className="mb-2 space-y-1">
                  {msg.tools.map((t, j) => (
                    <div
                      key={j}
                      className="flex items-center gap-2 text-xs text-muted-foreground"
                    >
                      {t.type === "tool_start" ? (
                        <>
                          <Wrench className="h-3 w-3" />
                          <span>
                            calling <code className="text-foreground">{t.name}</code>
                          </span>
                          {t.input?.app_id && (
                            <Badge variant="outline" className="text-[10px]">
                              {t.input.app_id}
                            </Badge>
                          )}
                        </>
                      ) : (
                        <>
                          <CheckCircle className="h-3 w-3 text-primary" />
                          <span className="truncate max-w-[400px]">
                            {t.output?.slice(0, 100)}
                          </span>
                        </>
                      )}
                    </div>
                  ))}
                </div>
              )}

              {/* Message content */}
              <div className="text-sm whitespace-pre-wrap break-words">
                {msg.content.split("```").map((block, j) => {
                  if (j % 2 === 0) return <span key={j}>{block}</span>;
                  return (
                    <pre
                      key={j}
                      className="my-2 p-3 bg-[#0d0d0d] border border-border overflow-x-auto text-xs"
                    >
                      <code>{block.replace(/^[a-z]*\n/, "")}</code>
                    </pre>
                  );
                })}
              </div>
            </div>
          </div>
        ))}

        {/* Streaming tool indicators */}
        {streaming && currentTools.length > 0 && (
          <div className="flex gap-3">
            <Bot className="h-4 w-4 text-primary mt-1 flex-shrink-0" />
            <div className="space-y-1">
              {currentTools
                .filter((t) => t.type === "tool_start")
                .map((t, i) => {
                  const hasEnd = currentTools.some(
                    (e) => e.type === "tool_end" && e.name === t.name
                  );
                  return (
                    <div
                      key={i}
                      className="flex items-center gap-2 text-xs text-muted-foreground"
                    >
                      {hasEnd ? (
                        <CheckCircle className="h-3 w-3 text-primary" />
                      ) : (
                        <Loader2 className="h-3 w-3 animate-spin" />
                      )}
                      <span>{t.name}</span>
                    </div>
                  );
                })}
            </div>
          </div>
        )}

        {streaming && (
          <div className="flex gap-3">
            <Bot className="h-4 w-4 text-primary mt-1 flex-shrink-0" />
            <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
          </div>
        )}

        <div ref={bottomRef} />
      </div>

      {/* Input */}
      <form onSubmit={handleSubmit} className="flex gap-2 pt-2 border-t border-border">
        <Textarea
          ref={textareaRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Describe what you want to build..."
          className="flex-1 min-h-[44px] max-h-[120px] resize-none bg-card"
          disabled={streaming}
        />
        <Button
          type="submit"
          disabled={streaming || !input.trim()}
          className="self-end"
        >
          {streaming ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Send className="h-4 w-4" />
          )}
        </Button>
      </form>
    </div>
  );
}
