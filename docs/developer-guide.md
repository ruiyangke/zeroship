# appbase Developer Guide

This guide covers everything you need to write apps for the appbase platform.

---

## App Structure

An appbase app is an ES module that exports functions. Each exported function is an RPC method that clients can call over HTTP.

```javascript
export function methodName(param1, param2) {
  return result;
}
```

That's it. No build step, no framework. The platform handles HTTP routing, JSON serialization, error handling, and metering.

### Minimal Example

```javascript
export function ping() {
  return "pong";
}

export function echo(message) {
  return message;
}
```

Deploy and call it:

```bash
# Deploy
curl -X POST http://localhost:3000/api/apps/my-app/deploy \
  -H "Authorization: Bearer $API_KEY" \
  -d @server.js

# Call
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: my-app" \
  -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'
# -> {"jsonrpc":"2.0","result":"pong","id":1}
```

### Important Rules

1. **Use `export function`** for each RPC method. The platform reads your module's exports to find callable methods.
2. **You can use `let`/`const`** freely — V8 runs your code in ES module mode.
3. **Return JSON-serializable values** — the result is serialized via `JSON.stringify`.
4. **Methods receive positional arguments** — `params: [1, "hello"]` maps to `function(a, b)` where `a=1, b="hello"`.

---

## Sync vs Async Handlers

Methods can be synchronous (return a value) or asynchronous (return a Promise). The runtime detects Promises automatically.

### Sync

```javascript
export function add(a, b) {
  return a + b;
}
```

### Async

```javascript
export async function fetchData(url) {
  const resp = await fetch(url);
  return await resp.json();
}
```

Both work seamlessly. The platform awaits Promises before serializing the response.

---

## Using fetch() for External APIs

The `fetch` API is available globally with the same interface as the Web Fetch API.

### Basic GET

```javascript
export async function getUser(id) {
  const resp = await fetch("https://jsonplaceholder.typicode.com/users/" + id);
  if (!resp.ok) {
    throw new Error("User not found: " + resp.status);
  }
  return await resp.json();
}
```

### POST with Headers

```javascript
export async function createPost(title, body) {
  const resp = await fetch("https://jsonplaceholder.typicode.com/posts", {
    method: "POST",
    headers: {
      "Content-Type": "application/json"
    },
    body: JSON.stringify({ title: title, body: body, userId: 1 })
  });
  return await resp.json();
}
```

### Security

- Only `http://` and `https://` URLs are allowed
- Private IPs (10.x, 192.168.x, 127.x, etc.) are blocked to prevent SSRF
- Maximum response body size: 10 MB

---

## Using kv for Data Storage

The `kv` object provides an in-memory key-value store. Data persists across requests for the lifetime of the isolate, but is lost when the isolate is evicted (idle timeout, restart, redeployment).

### API

| Method | Signature | Returns |
|--------|-----------|---------|
| `kv.get(key)` | `(string) -> string \| null` | Value or `null` if not found |
| `kv.set(key, value)` | `(string, string) -> void` | — |
| `kv.delete(key)` | `(string) -> void` | — |
| `kv.list()` | `() -> string[]` | All keys |

### Example: View Counter

```javascript
export function visit(page) {
  const key = "views:" + (page || "home");
  const count = parseInt(kv.get(key) || "0") + 1;
  kv.set(key, String(count));
  return { page: page || "home", views: count };
}

export function stats() {
  const keys = kv.list();
  const result = {};
  for (let i = 0; i < keys.length; i++) {
    result[keys[i]] = parseInt(kv.get(keys[i]) || "0");
  }
  return result;
}
```

### Important

- Keys and values must be strings. Serialize complex data with `JSON.stringify`.
- Data is not shared between apps (each isolate has its own namespace).
- Data does not survive isolate eviction. For persistent storage, use external databases via `fetch()`.

---

## Using env for Configuration

The `env` object provides read-only access to per-app configuration via process environment variables.

### API

| Method | Signature | Returns |
|--------|-----------|---------|
| `env.get(key)` | `(string) -> string \| null` | Value or `null` if not set |

Keys are case-insensitive and mapped to process environment variables with the `APPBASE_APP_` prefix. For example, `env.get("api_key")` reads `APPBASE_APP_API_KEY`.

### Example: API Proxy with Secret Key

```bash
# Set the env var before starting the server
export APPBASE_APP_OWM_KEY="your-openweathermap-key"
```

```javascript
export async function weather(city) {
  const apiKey = env.get("owm_key");
  if (!apiKey) throw new Error("OWM_KEY not configured");

  const resp = await fetch(
    "https://api.openweathermap.org/data/2.5/weather?q=" +
    encodeURIComponent(city) + "&appid=" + apiKey + "&units=metric"
  );
  if (!resp.ok) throw new Error("API error: " + resp.status);
  return await resp.json();
}
```

### Important

- Only variables with the `APPBASE_APP_` prefix are accessible. System variables like `PATH` or `HOME` are never exposed.
- Values are read from the process environment at call time — no restart required if you change them.
- Returns `null` (not `undefined`) when a key is not set.

---

## Utility APIs

### crypto.randomUUID()

Generates a RFC 4122 v4 UUID string.

```javascript
export function create(name) {
  const id = crypto.randomUUID();
  // e.g. "550e8400-e29b-41d4-a716-446655440000"
  kv.set("item:" + id, JSON.stringify({ id: id, name: name }));
  return { id: id };
}
```

### TextEncoder / TextDecoder

Encode and decode UTF-8 strings to/from `Uint8Array`.

```javascript
var enc = new TextEncoder();
var buf = enc.encode("Hello");    // Uint8Array [72, 101, 108, 108, 111]

var dec = new TextDecoder();
var str = dec.decode(buf);        // "Hello"
```

### btoa / atob

Base64 encode and decode strings.

```javascript
var encoded = btoa("Hello, World!");  // "SGVsbG8sIFdvcmxkIQ=="
var decoded = atob(encoded);          // "Hello, World!"
```

### structuredClone

Deep-clone a JSON-serializable object. This is a simplified polyfill using `JSON.parse(JSON.stringify(...))` — it handles plain objects, arrays, strings, numbers, booleans, and null. It does not preserve `Date`, `Map`, `Set`, `RegExp`, or circular references.

```javascript
var original = { a: 1, b: [2, 3] };
var clone = structuredClone(original);
clone.b.push(4);
// original.b is still [2, 3], clone.b is [2, 3, 4]
```

---

## Error Handling

Thrown errors and rejected Promises are caught by the runtime and returned as JSON-RPC errors.

### Throwing Errors

```javascript
export function divide(a, b) {
  if (b === 0) {
    throw new Error("Division by zero");
  }
  return a / b;
}
```

Response:

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32000,
    "message": "Division by zero"
  },
  "id": 1
}
```

### Async Error Handling

```javascript
export async function safeGet(url) {
  try {
    const resp = await fetch(url);
    if (!resp.ok) {
      return { error: "HTTP " + resp.status, url: url };
    }
    return await resp.json();
  } catch (e) {
    return { error: e.message, url: url };
  }
}
```

### Error Codes

| Code | Meaning |
|------|---------|
| `-32601` | Method not found (no matching export in module) |
| `-32000` | Application error (your code threw or rejected) |
| `-32603` | Internal server error |
| `-32001` | Rate limit exceeded |
| `-32002` | Concurrency limit exceeded |
| `-32003` | Spending limit reached |
| `-32004` | Quota exceeded |

---

## Limits

### Per-Request Limits

| Resource | Free Tier | Pro Tier |
|----------|-----------|----------|
| CPU time | 10 ms | 30,000 ms |
| Wall time | 30 s (hard) | 30 s (hard) |
| Response size | 10 MB | 10 MB |
| Concurrent requests | 100 | 100 |

### Monthly Quotas (Free Tier)

| Resource | Limit |
|----------|-------|
| Total CPU time | 50,000 ms |
| Requests | 100,000 |
| Egress bytes | 1 GB |
| DB reads | 500,000 |
| DB writes | 50,000 |
| KV reads | 100,000 |
| KV writes | 100,000 |

### Rate Limits

| Plan | Requests/sec | Burst |
|------|-------------|-------|
| Free | 10 | 50 |
| Pro | 1,000 | 5,000 |

When quotas are exceeded, requests return `429 Too Many Requests` with a `Retry-After` header indicating seconds until the monthly reset. Warning headers appear at 80% usage.

---

## Examples

### Todo API

An in-memory todo list with full CRUD.

```javascript
let todos = [];
let nextId = 1;

export function list() {
  return todos;
}

export function add(title) {
  if (!title) throw new Error("title is required");
  const todo = { id: nextId++, title: title, done: false };
  todos.push(todo);
  return todo;
}

export function toggle(id) {
  const todo = todos.find(t => t.id === id);
  if (!todo) throw new Error("Todo not found: " + id);
  todo.done = !todo.done;
  return todo;
}

export function remove(id) {
  const idx = todos.findIndex(t => t.id === id);
  if (idx === -1) throw new Error("Todo not found: " + id);
  return todos.splice(idx, 1)[0];
}
```

```bash
# Add a todo
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: todo-app" \
  -d '{"jsonrpc":"2.0","method":"add","params":["Buy milk"],"id":1}'

# List todos
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: todo-app" \
  -d '{"jsonrpc":"2.0","method":"list","params":[],"id":2}'

# Toggle done
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: todo-app" \
  -d '{"jsonrpc":"2.0","method":"toggle","params":[1],"id":3}'
```

---

### URL Shortener

Uses the KV store for persistence (within isolate lifetime).

```javascript
const CHARS = "abcdefghijklmnopqrstuvwxyz0123456789";

function randomCode(len) {
  let code = "";
  for (let i = 0; i < (len || 6); i++) {
    code += CHARS[Math.floor(Math.random() * CHARS.length)];
  }
  return code;
}

export function shorten(url) {
  if (!url) throw new Error("url is required");
  // Check if already shortened
  const existing = kv.get("url:" + url);
  if (existing) return { code: existing, short: "/" + existing };

  const code = randomCode(6);
  kv.set("code:" + code, url);
  kv.set("url:" + url, code);
  return { code: code, short: "/" + code };
}

export function resolve(code) {
  if (!code) throw new Error("code is required");
  const url = kv.get("code:" + code);
  if (!url) throw new Error("Not found: " + code);
  return { code: code, url: url };
}

export function stats() {
  const keys = kv.list();
  let count = 0;
  for (let i = 0; i < keys.length; i++) {
    if (keys[i].indexOf("code:") === 0) count++;
  }
  return { total_links: count };
}
```

```bash
# Shorten a URL
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: shortener" \
  -d '{"jsonrpc":"2.0","method":"shorten","params":["https://example.com/very/long/path"],"id":1}'

# Resolve a code
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: shortener" \
  -d '{"jsonrpc":"2.0","method":"resolve","params":["abc123"],"id":2}'
```

---

### API Proxy

Forward requests to a third-party API, adding authentication and transforming the response.

```javascript
const API_BASE = "https://api.openweathermap.org/data/2.5";

export async function weather(city) {
  if (!city) throw new Error("city is required");

  // Use the platform env system for secrets
  // Set APPBASE_OWM_KEY in your environment before starting the server
  const apiKey = env.get("owm_key") || "demo"; // reads APPBASE_APP_OWM_KEY from process env

  const resp = await fetch(
    API_BASE + "/weather?q=" + encodeURIComponent(city) + "&appid=" + apiKey + "&units=metric"
  );

  if (!resp.ok) {
    const err = await resp.text();
    throw new Error("API error " + resp.status + ": " + err);
  }

  const data = await resp.json();
  return {
    city: data.name,
    country: data.sys.country,
    temp: data.main.temp,
    feels_like: data.main.feels_like,
    humidity: data.main.humidity,
    description: data.weather[0].description,
    wind_speed: data.wind.speed
  };
}

export async function forecast(city, days) {
  if (!city) throw new Error("city is required");
  days = days || 3;

  const resp = await fetch(
    "https://wttr.in/" + encodeURIComponent(city) + "?format=j1"
  );

  if (!resp.ok) throw new Error("Forecast API error: " + resp.status);

  const data = await resp.json();
  const result = [];
  const weather = data.weather || [];
  for (let i = 0; i < Math.min(days, weather.length); i++) {
    const day = weather[i];
    result.push({
      date: day.date,
      max_temp_c: day.maxtempC,
      min_temp_c: day.mintempC,
      description: day.hourly[4].weatherDesc[0].value
    });
  }
  return result;
}
```

```bash
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: weather" \
  -d '{"jsonrpc":"2.0","method":"forecast","params":["Tokyo",5],"id":1}'
```

---

### Webhook Handler

Receive webhook data, validate it, and forward to a downstream service.

```javascript
// Receive a webhook payload, log it, and forward to Slack
export async function receive(payload) {
  if (!payload) throw new Error("Empty payload");

  // Log the webhook
  const count = parseInt(kv.get("webhook_count") || "0") + 1;
  kv.set("webhook_count", String(count));
  kv.set("last_webhook", JSON.stringify(payload));

  console.log("Webhook #" + count + ": " + JSON.stringify(payload).substring(0, 200));

  // Forward to Slack (replace with your webhook URL)
  const slackUrl = "https://hooks.slack.com/services/YOUR/WEBHOOK/URL";
  const message = {
    text: "Webhook received: " + (payload.event || payload.type || "unknown"),
    blocks: [
      {
        type: "section",
        text: {
          type: "mrkdwn",
          text: "*Webhook #" + count + "*\n```" + JSON.stringify(payload, null, 2).substring(0, 500) + "```"
        }
      }
    ]
  };

  try {
    await fetch(slackUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(message)
    });
    return { received: true, count: count };
  } catch (e) {
    return { received: true, count: count, forward_error: e.message };
  }
}

// Check recent webhook stats
export function stats() {
  return {
    total: parseInt(kv.get("webhook_count") || "0"),
    last: JSON.parse(kv.get("last_webhook") || "null")
  };
}
```

```bash
# Simulate a webhook
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: webhook-handler" \
  -d '{"jsonrpc":"2.0","method":"receive","params":[{"event":"push","repo":"my-app","branch":"main"}],"id":1}'

# Check stats
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: webhook-handler" \
  -d '{"jsonrpc":"2.0","method":"stats","params":[],"id":2}'
```

---

## Full Workflow: Create, Deploy, Call

Here is the complete lifecycle for creating and using an app:

```bash
# 1. Create the app
curl -X POST http://localhost:3000/api/apps \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"id":"my-app","plan_id":"free"}'
# Save the api_key from the response

# 2. Write your code
cat > server.js << 'EOF'
export function greet(name) {
  return "Hello, " + (name || "world") + "!";
}
EOF

# 3. Deploy
curl -X POST http://localhost:3000/api/apps/my-app/deploy \
  -H "Authorization: Bearer $APP_API_KEY" \
  -d @server.js

# 4. Call your method
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: my-app" \
  -d '{"jsonrpc":"2.0","method":"greet","params":["Alice"],"id":1}'
# -> {"jsonrpc":"2.0","result":"Hello, Alice!","id":1}

# 5. Check logs
curl http://localhost:3000/api/apps/my-app/logs \
  -H "Authorization: Bearer $MASTER_KEY"

# 6. Check usage
curl http://localhost:3000/_apps/my-app/usage
```

---

## Tips

- **Keep apps small and focused.** One concern per app. A URL shortener, a webhook handler, a proxy — not all three in one file.
- **Validate all inputs.** Methods receive arbitrary data from the network. Check types and ranges.
- **Use `console.log` for debugging.** Logs are captured and viewable via the API and dashboard.
- **Handle fetch errors.** Network calls can fail. Always check `resp.ok` or wrap in try/catch.
- **Stringify complex KV values.** `kv.set("data", JSON.stringify(obj))` and `JSON.parse(kv.get("data"))`.
- **Redeploy to reset state.** Deploying new code evicts the old isolate, clearing all in-memory state (KV, variables, timers).
