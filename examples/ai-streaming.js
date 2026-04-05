// AI Streaming — Server-Sent Events proxy for OpenAI-compatible APIs
//
// Demonstrates: fetch → ReadableStream → SSE response
// The response streams tokens to the client as they arrive from the AI API.

export async function onRequest(request) {
    const url = new URL(request.url);

    if (url.pathname === "/chat" && request.method === "POST") {
        const body = JSON.parse(request._bodyText || "{}");
        const prompt = body.prompt || "Say hello";
        const model = body.model || "gpt-3.5-turbo";

        // Call OpenAI-compatible API
        const apiKey = env.get("OPENAI_API_KEY") || "test-key";
        const apiUrl = body.api_url || "https://api.openai.com/v1/chat/completions";

        const aiResponse = await fetch(apiUrl, {
            method: "POST",
            headers: {
                "Content-Type": "application/json",
                "Authorization": "Bearer " + apiKey,
            },
            body: JSON.stringify({
                model: model,
                messages: [{ role: "user", content: prompt }],
                stream: true,
            }),
        });

        // For now, our fetch returns the full body as a string.
        // Parse the SSE events and re-stream them to the client.
        const responseText = aiResponse._bodyText || "";
        const lines = responseText.split("\n");

        const stream = new ReadableStream({
            start(controller) {
                for (const line of lines) {
                    if (line.startsWith("data: ")) {
                        const data = line.slice(6).trim();
                        if (data === "[DONE]") {
                            controller.enqueue("data: [DONE]\n\n");
                            break;
                        }
                        try {
                            const parsed = JSON.parse(data);
                            const content = parsed.choices &&
                                parsed.choices[0] &&
                                parsed.choices[0].delta &&
                                parsed.choices[0].delta.content;
                            if (content) {
                                controller.enqueue("data: " + JSON.stringify({ content }) + "\n\n");
                            }
                        } catch (e) {
                            // Skip malformed SSE lines
                        }
                    }
                }
                controller.close();
            }
        });

        return new Response(stream, {
            status: 200,
            headers: {
                "Content-Type": "text/event-stream",
                "Cache-Control": "no-cache",
                "Connection": "keep-alive",
            },
        });
    }

    // Simple non-streaming endpoint for testing
    if (url.pathname === "/complete") {
        const body = JSON.parse(request._bodyText || "{}");
        const prompt = body.prompt || "Say hello";

        // Simulate AI response without external API
        const tokens = prompt.split(" ");
        const stream = new ReadableStream({
            start(controller) {
                for (let i = 0; i < tokens.length; i++) {
                    const chunk = {
                        content: tokens[i] + (i < tokens.length - 1 ? " " : ""),
                        index: i,
                    };
                    controller.enqueue("data: " + JSON.stringify(chunk) + "\n\n");
                }
                controller.enqueue("data: [DONE]\n\n");
                controller.close();
            }
        });

        return new Response(stream, {
            status: 200,
            headers: { "Content-Type": "text/event-stream" },
        });
    }

    return new Response(JSON.stringify({
        endpoints: [
            "POST /chat   — proxy to OpenAI with SSE streaming",
            "POST /complete — local echo with SSE streaming (no API key needed)",
        ]
    }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
    });
}

// RPC method for testing
export function ping() { return "pong"; }
