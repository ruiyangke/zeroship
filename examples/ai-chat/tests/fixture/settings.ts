// Shared contract for the ai-chat browser suite.
//
// The app server, the OpenAI-compatible stub that stands in for the provider,
// and the spec all read these values, so the request the app makes and the
// stream the spec asserts on are described once.

/** The Vite dev server the fixture starts. */
export const APP_ORIGIN = "http://127.0.0.1:5173";

/**
 * The fixture's readiness gate. Vite answers `/` long before the runtime
 * behind it serves, so Playwright waits on this port instead: it opens only
 * once an RPC reaches the runtime rather than the dev-server's 503.
 */
export const GATE_ORIGIN = "http://127.0.0.1:5179";

/**
 * Where the stub listens. A fixed port is the one piece of configuration that
 * cannot travel through the app itself: the fixture binds it before the app
 * starts, and the spec reaches its capture endpoint directly. A mismatch
 * between the two is caught by the suite, because the spec's `/__requests`
 * call would fail rather than silently observing nothing.
 */
export const STUB_ORIGIN = "http://127.0.0.1:5178";

/** The OpenAI-compatible base URL the app is configured with. */
export const STUB_API_BASE = `${STUB_ORIGIN}/v1`;

/**
 * A placeholder key. The app reads it from its own environment and sends it as
 * `Authorization: Bearer ...`; the stub rejects anything else, so the suite
 * proves the configured key reached the provider.
 */
export const STUB_API_KEY = "stub-key-no-network";

/** The model the app asks for. */
export const MODEL = "gpt-5-nano";

/** A prompt whose reply the stub streams as several text deltas. */
export const STREAM_PROMPT = "Say hello.";

/** The deltas the stub streams for `STREAM_PROMPT`, in order. */
export const STREAM_CHUNKS = ["Hello", " from", " the", " stub", "!"] as const;

/** What the client must render once it concatenates every delta. */
export const STREAM_REPLY = STREAM_CHUNKS.join("");

/** A prompt that makes the stub answer with a provider error. */
export const ERROR_PROMPT = "Fail this request.";

/** The provider error message the stub returns; the UI must surface it. */
export const ERROR_MESSAGE = "stub provider is unavailable";

/**
 * Model-message content is either a string or a list of parts. Both the stub
 * and the spec need the text, so extraction lives here rather than in two
 * places that could disagree.
 */
export function messageText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      if (part && typeof part === "object" && typeof (part as { text?: unknown }).text === "string") {
        return (part as { text: string }).text;
      }
      return "";
    })
    .join("");
}
