// Shared utilities

export function jsonResponse(data: unknown, status = 200): Response {
    return new Response(JSON.stringify(data), {
        status,
        headers: { "Content-Type": "application/json" },
    });
}

export function errorResponse(message: string, status = 500): Response {
    return jsonResponse({ error: message }, status);
}

// This function is NOT imported by anyone — should be tree-shaken
export function unusedHelper(): string {
    return "this function should be eliminated by tree-shaking";
}
