# appbase Developer Guide

This guide covers everything you need to write apps for the appbase platform.

---

## App Structure

An appbase app is a single JavaScript file that defines a `var __rpc` object on the global scope. Each property of `__rpc` is an RPC method that clients can call over HTTP.

```javascript
var __rpc = {
  methodName: function(param1, param2) {
    return result;
  }
};
```

That's it. No imports, no build step, no framework. The platform handles HTTP routing, JSON serialization, error handling, and metering.

### Minimal Example

```javascript
var __rpc = {
  ping: function() {
    return "pong";
  },
  echo: function(message) {
    return message;
  }
};
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

1. **Use `var`, not `let` or `const`** for top-level declarations. V8 runs your code in classic script mode, not module mode. `let`/`const` at the top level will not attach to `globalThis`.
2. **`__rpc` must be a plain object** — the platform reads its properties to find callable methods.
3. **Return JSON-serializable values** — the result is serialized via `JSON.stringify`.
4. **Methods receive positional arguments** — `params: [1, "hello"]` maps to `function(a, b)` where `a=1, b="hello"`.

---

## Sync vs Async Handlers

Methods can be synchronous (return a value) or asynchronous (return a Promise). The runtime detects Promises automatically.

### Sync

```javascript
var __rpc = {
  add: function(a, b) {
    return a + b;
  }
};
```

### Async

```javascript
var __rpc = {
  fetchData: async function(url) {
    var resp = await fetch(url);
    return await resp.json();
  }
};
```

Both work seamlessly. The platform awaits Promises before serializing the response.

---

## Using fetch() for External APIs

The `fetch` API is available globally with the same interface as the Web Fetch API.

### Basic GET

```javascript
var __rpc = {
  getUser: async function(id) {
    var resp = await fetch("https://jsonplaceholder.typicode.com/users/" + id);
    if (!resp.ok) {
      throw new Error("User not found: " + resp.status);
    }
    return await resp.json();
  }
};
```

### POST with Headers

```javascript
var __rpc = {
  createPost: async function(title, body) {
    var resp = await fetch("https://jsonplaceholder.typicode.com/posts", {
      method: "POST",
      headers: {
        "Content-Type": "application/json"
      },
      body: JSON.stringify({ title: title, body: body, userId: 1 })
    });
    return await resp.json();
  }
};
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
var __rpc = {
  visit: function(page) {
    var key = "views:" + (page || "home");
    var count = parseInt(kv.get(key) || "0") + 1;
    kv.set(key, String(count));
    return { page: page || "home", views: count };
  },

  stats: function() {
    var keys = kv.list();
    var result = {};
    for (var i = 0; i < keys.length; i++) {
      result[keys[i]] = parseInt(kv.get(keys[i]) || "0");
    }
    return result;
  }
};
```

### Important

- Keys and values must be strings. Serialize complex data with `JSON.stringify`.
- Data is not shared between apps (each isolate has its own namespace).
- Data does not survive isolate eviction. For persistent storage, use external databases via `fetch()`.

---

## Error Handling

Thrown errors and rejected Promises are caught by the runtime and returned as JSON-RPC errors.

### Throwing Errors

```javascript
var __rpc = {
  divide: function(a, b) {
    if (b === 0) {
      throw new Error("Division by zero");
    }
    return a / b;
  }
};
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
var __rpc = {
  safeGet: async function(url) {
    try {
      var resp = await fetch(url);
      if (!resp.ok) {
        return { error: "HTTP " + resp.status, url: url };
      }
      return await resp.json();
    } catch (e) {
      return { error: e.message, url: url };
    }
  }
};
```

### Error Codes

| Code | Meaning |
|------|---------|
| `-32601` | Method not found (no matching key in `__rpc`) |
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
var todos = [];
var nextId = 1;

var __rpc = {
  list: function() {
    return todos;
  },

  add: function(title) {
    if (!title) throw new Error("title is required");
    var todo = { id: nextId++, title: title, done: false };
    todos.push(todo);
    return todo;
  },

  toggle: function(id) {
    var todo = todos.find(function(t) { return t.id === id; });
    if (!todo) throw new Error("Todo not found: " + id);
    todo.done = !todo.done;
    return todo;
  },

  remove: function(id) {
    var idx = todos.findIndex(function(t) { return t.id === id; });
    if (idx === -1) throw new Error("Todo not found: " + id);
    return todos.splice(idx, 1)[0];
  }
};
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
var CHARS = "abcdefghijklmnopqrstuvwxyz0123456789";

function randomCode(len) {
  var code = "";
  for (var i = 0; i < (len || 6); i++) {
    code += CHARS[Math.floor(Math.random() * CHARS.length)];
  }
  return code;
}

var __rpc = {
  shorten: function(url) {
    if (!url) throw new Error("url is required");
    // Check if already shortened
    var existing = kv.get("url:" + url);
    if (existing) return { code: existing, short: "/" + existing };

    var code = randomCode(6);
    kv.set("code:" + code, url);
    kv.set("url:" + url, code);
    return { code: code, short: "/" + code };
  },

  resolve: function(code) {
    if (!code) throw new Error("code is required");
    var url = kv.get("code:" + code);
    if (!url) throw new Error("Not found: " + code);
    return { code: code, url: url };
  },

  stats: function() {
    var keys = kv.list();
    var count = 0;
    for (var i = 0; i < keys.length; i++) {
      if (keys[i].indexOf("code:") === 0) count++;
    }
    return { total_links: count };
  }
};
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
var API_BASE = "https://api.openweathermap.org/data/2.5";

var __rpc = {
  weather: async function(city) {
    if (!city) throw new Error("city is required");

    // Use the platform env system for secrets
    // Set APPBASE_OWM_KEY in your environment before starting the server
    var apiKey = "demo"; // Replace with env.get("APPBASE_OWM_KEY") when env plugin is wired

    var resp = await fetch(
      API_BASE + "/weather?q=" + encodeURIComponent(city) + "&appid=" + apiKey + "&units=metric"
    );

    if (!resp.ok) {
      var err = await resp.text();
      throw new Error("API error " + resp.status + ": " + err);
    }

    var data = await resp.json();
    return {
      city: data.name,
      country: data.sys.country,
      temp: data.main.temp,
      feels_like: data.main.feels_like,
      humidity: data.main.humidity,
      description: data.weather[0].description,
      wind_speed: data.wind.speed
    };
  },

  forecast: async function(city, days) {
    if (!city) throw new Error("city is required");
    days = days || 3;

    var resp = await fetch(
      "https://wttr.in/" + encodeURIComponent(city) + "?format=j1"
    );

    if (!resp.ok) throw new Error("Forecast API error: " + resp.status);

    var data = await resp.json();
    var result = [];
    var weather = data.weather || [];
    for (var i = 0; i < Math.min(days, weather.length); i++) {
      var day = weather[i];
      result.push({
        date: day.date,
        max_temp_c: day.maxtempC,
        min_temp_c: day.mintempC,
        description: day.hourly[4].weatherDesc[0].value
      });
    }
    return result;
  }
};
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
var __rpc = {
  // Receive a webhook payload, log it, and forward to Slack
  receive: async function(payload) {
    if (!payload) throw new Error("Empty payload");

    // Log the webhook
    var count = parseInt(kv.get("webhook_count") || "0") + 1;
    kv.set("webhook_count", String(count));
    kv.set("last_webhook", JSON.stringify(payload));

    console.log("Webhook #" + count + ": " + JSON.stringify(payload).substring(0, 200));

    // Forward to Slack (replace with your webhook URL)
    var slackUrl = "https://hooks.slack.com/services/YOUR/WEBHOOK/URL";
    var message = {
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
  },

  // Check recent webhook stats
  stats: function() {
    return {
      total: parseInt(kv.get("webhook_count") || "0"),
      last: JSON.parse(kv.get("last_webhook") || "null")
    };
  }
};
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
var __rpc = {
  greet: function(name) {
    return "Hello, " + (name || "world") + "!";
  }
};
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
