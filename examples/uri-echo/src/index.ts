// uri-echo — reflects the EXACT URL the worker's V8 sees back to the caller,
// so a test can assert what the gateway forwarded (path, encoding, query).
//
// A plain `{ fetch }` app: URL resources default to anon/public, so every path
// dispatches to this handler through the gateway without a session.
export default {
  fetch(request: Request): Response {
    const u = new URL(request.url);
    const body = {
      // The raw URL string as the runtime built it from the gateway envelope.
      url: request.url,
      method: request.method,
      // Parsed via WHATWG URL — what app code actually reads.
      pathname: u.pathname,
      // `search` includes the leading "?" (or "" when absent).
      search: u.search,
      // Ordered key/value pairs — preserves repeats AND order.
      params: [...u.searchParams.entries()],
      // A couple of named lookups for convenience in assertions.
      q: u.searchParams.get("q"),
      all_q: u.searchParams.getAll("q"),
    };
    return new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json" },
    });
  },
};
