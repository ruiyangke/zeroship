# appbase API Reference

All API endpoints served by the appbase HTTP server. The server listens on `http://0.0.0.0:3000` by default.

---

## Data Plane

### POST /rpc

Call an app's RPC method using JSON-RPC 2.0.

**App Resolution** (checked in order):
1. Subdomain: `my-app.platform.dev` resolves to app `my-app`
2. Header: `X-App-Id: my-app`
3. Default: falls back to the `default` app

**Request Headers:**

| Header | Required | Description |
|--------|----------|-------------|
| `Content-Type` | Yes | `application/json` |
| `X-App-Id` | No | Target app ID (if not using subdomain routing) |

**Request Body:**

```json
{
  "jsonrpc": "2.0",
  "method": "methodName",
  "params": [arg1, arg2],
  "id": 1
}
```

| Field | Type | Description |
|-------|------|-------------|
| `jsonrpc` | string | Must be `"2.0"` |
| `method` | string | Name of the RPC method defined in the app's `__rpc` object |
| `params` | array | Arguments passed to the method |
| `id` | number/string | Request ID, echoed back in the response |

**Success Response (200):**

```json
{
  "jsonrpc": "2.0",
  "result": "pong",
  "id": 1
}
```

**Response Headers:**

| Header | Description |
|--------|-------------|
| `x-cpu-time-ms` | CPU time consumed by this request (ms) |
| `x-wall-time-ms` | Wall clock time for this request (ms) |
| `x-plan` | App's current plan name |
| `ratelimit` | IETF RateLimit header: `limit=N, remaining=N, reset=N` |
| `ratelimit-policy` | IETF RateLimit policy: `N;w=N` |
| `x-quota-warning` | Present when any quota dimension exceeds 80% usage |
| `x-spending-warning` | Present when approaching spending limit |

**Error Responses:**

| Status | Code | Reason |
|--------|------|--------|
| 429 | `-32001` | Rate limit exceeded |
| 429 | `-32002` | Concurrency limit (>100 in-flight) |
| 429 | `-32003` | Spending limit reached |
| 429 | `-32004` | Monthly quota exceeded |
| 404 | — | App not found |
| 500 | `-32000` | JS runtime error |
| 500 | `-32603` | Internal server error |

Error responses include a `Retry-After` header with seconds until the limit resets.

**Error Response Body:**

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32004,
    "message": "Quota exceeded: requests (100000/100000)",
    "data": {
      "type": "quota_exceeded",
      "dimension": "requests",
      "used": 100000,
      "limit": 100000
    }
  },
  "id": null
}
```

**Example:**

```bash
# Call a sync method
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: my-app" \
  -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'

# Call an async method with arguments
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: weather-app" \
  -d '{"jsonrpc":"2.0","method":"get","params":["London"],"id":1}'
```

---

## Control Plane API

All control plane endpoints require authentication via the `Authorization` header. Admin endpoints require the master key. Deploy endpoints accept either the master key or the app's per-app API key.

**Authentication:**

```
Authorization: Bearer <master-key-or-app-key>
```

In development mode (when `APPBASE_MASTER_KEY` is not set), authentication is skipped.

---

### POST /api/apps

Create a new app.

**Auth:** Master key required.

**Request Body:**

```json
{
  "id": "my-app",
  "plan_id": "free"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | Yes | — | App ID (1-64 chars, alphanumeric, hyphens, underscores) |
| `plan_id` | string | No | `"free"` | Plan to assign |

**Success Response (201):**

```json
{
  "id": "my-app",
  "plan_id": "free",
  "version": 0,
  "api_key": "550e8400-e29b-41d4-a716-446655440000",
  "created_at": "2026-04-01 12:00:00",
  "updated_at": "2026-04-01 12:00:00"
}
```

Save the `api_key` — it is used to deploy code to this app.

**Error Responses:**

| Status | Reason |
|--------|--------|
| 400 | Invalid app ID format or bad JSON |
| 401 | Invalid or missing master key |
| 409 | App already exists |

**Example:**

```bash
curl -X POST http://localhost:3000/api/apps \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"id":"my-app","plan_id":"free"}'
```

---

### GET /api/apps

List all apps.

**Auth:** Master key required.

**Success Response (200):**

```json
[
  {
    "id": "default",
    "plan_id": "pro",
    "version": 3,
    "api_key": "...",
    "created_at": "2026-04-01 12:00:00",
    "updated_at": "2026-04-01 12:30:00"
  },
  {
    "id": "my-app",
    "plan_id": "free",
    "version": 1,
    "api_key": "...",
    "created_at": "2026-04-01 12:15:00",
    "updated_at": "2026-04-01 12:15:00"
  }
]
```

**Example:**

```bash
curl http://localhost:3000/api/apps \
  -H "Authorization: Bearer $MASTER_KEY"
```

---

### GET /api/apps/:id

Get a single app's details, including its deployed code.

**Auth:** Master key required.

**Success Response (200):**

```json
{
  "id": "my-app",
  "plan_id": "free",
  "version": 2,
  "server_js": "var __rpc = { ping: function() { return \"pong\"; } };"
}
```

**Error Responses:**

| Status | Reason |
|--------|--------|
| 401 | Invalid or missing master key |
| 404 | App not found |

**Example:**

```bash
curl http://localhost:3000/api/apps/my-app \
  -H "Authorization: Bearer $MASTER_KEY"
```

---

### DELETE /api/apps/:id

Delete an app permanently. Evicts its isolate, clears cached bundles, and removes usage data.

**Auth:** Master key required.

**Success Response (200):**

```json
{
  "deleted": "my-app"
}
```

**Error Responses:**

| Status | Reason |
|--------|--------|
| 401 | Invalid or missing master key |
| 404 | App not found |

**Example:**

```bash
curl -X DELETE http://localhost:3000/api/apps/my-app \
  -H "Authorization: Bearer $MASTER_KEY"
```

---

### POST /api/apps/:id/deploy

Deploy JavaScript code to an app. The request body is the raw JS source code (not JSON).

On deploy, the app's cached bundle is invalidated and its V8 isolate is evicted. The next request will create a fresh isolate with the new code.

**Auth:** Per-app API key (returned from `POST /api/apps`) or master key.

**Request Headers:**

| Header | Required | Description |
|--------|----------|-------------|
| `Authorization` | Yes | `Bearer <api-key>` or `Bearer <master-key>` |

**Request Body:** Raw JavaScript source code (plain text).

```javascript
var __rpc = {
  ping: function() {
    return "pong";
  }
};
```

**Success Response (200):**

```json
{
  "version": 3
}
```

**Error Responses:**

| Status | Reason |
|--------|--------|
| 400 | Empty deploy body |
| 401 | Missing or invalid API key |
| 404 | App not found |

**Example:**

```bash
# Deploy using the app's API key
curl -X POST http://localhost:3000/api/apps/my-app/deploy \
  -H "Authorization: Bearer 550e8400-e29b-41d4-a716-446655440000" \
  -d 'var __rpc = { ping: function() { return "pong"; } };'

# Deploy using the master key
curl -X POST http://localhost:3000/api/apps/my-app/deploy \
  -H "Authorization: Bearer $MASTER_KEY" \
  -d @server.js
```

---

### PUT /api/apps/:id/plan

Change an app's plan.

**Auth:** Master key required.

**Request Body:**

```json
{
  "plan_id": "pro"
}
```

**Success Response (200):**

```json
{
  "plan_id": "pro"
}
```

**Error Responses:**

| Status | Reason |
|--------|--------|
| 400 | Invalid JSON |
| 401 | Invalid or missing master key |
| 404 | App not found |

**Example:**

```bash
curl -X PUT http://localhost:3000/api/apps/my-app/plan \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"plan_id":"pro"}'
```

---

### GET /api/apps/:id/logs

Get recent console output for an app (last 100 log entries from the active isolate).

**Auth:** Master key required.

**Success Response (200):**

```json
[
  "Server functions registered: ping, hello",
  "Request received for user: alice"
]
```

Returns an empty array `[]` if the app has no active isolate or no logs.

**Example:**

```bash
curl http://localhost:3000/api/apps/my-app/logs \
  -H "Authorization: Bearer $MASTER_KEY"
```

---

### GET /api/templates

List available starter templates. No authentication required.

**Success Response (200):**

```json
[
  {
    "id": "hello-world",
    "name": "Hello World",
    "description": "Simple ping/pong API",
    "code": "var __rpc = { ... };"
  },
  {
    "id": "todo-api",
    "name": "Todo API",
    "description": "In-memory todo list with CRUD operations",
    "code": "..."
  },
  {
    "id": "weather-proxy",
    "name": "Weather Proxy",
    "description": "Fetch weather data from wttr.in",
    "code": "..."
  },
  {
    "id": "math-api",
    "name": "Math API",
    "description": "Basic math operations",
    "code": "..."
  }
]
```

**Example:**

```bash
curl http://localhost:3000/api/templates
```

---

## Admin API

Admin endpoints provide operational visibility and control. In development mode (default master key), no authentication is required. In production, these endpoints should be network-restricted or protected by a reverse proxy.

---

### GET /_health

Health check endpoint.

**Success Response (200):**

```json
{
  "status": "ok"
}
```

**Example:**

```bash
curl http://localhost:3000/_health
```

---

### GET /_stats

V8 isolate pool statistics.

**Success Response (200):**

```json
{
  "active_isolates": 3,
  "max_isolates": 1000,
  "apps": [
    {
      "app_id": "default",
      "total_cpu_ms": 0.0,
      "request_count": 42,
      "idle_secs": 5.2
    },
    {
      "app_id": "my-app",
      "total_cpu_ms": 0.0,
      "request_count": 7,
      "idle_secs": 120.0
    }
  ]
}
```

**Example:**

```bash
curl http://localhost:3000/_stats
```

---

### GET /_usage

Aggregated usage counters for all apps.

**Success Response (200):**

```json
{
  "default": {
    "requests": 1500,
    "cpu_us": 45000,
    "egress_bytes": 250000,
    "ingress_bytes": 50000,
    "concurrent_requests": 2
  },
  "my-app": {
    "requests": 300,
    "cpu_us": 12000,
    "egress_bytes": 80000,
    "ingress_bytes": 15000,
    "concurrent_requests": 0
  }
}
```

**Example:**

```bash
curl http://localhost:3000/_usage
```

---

### GET /_apps/:id/usage

Usage counters for a single app.

**Success Response (200):**

```json
{
  "requests": 1500,
  "cpu_us": 45000,
  "egress_bytes": 250000,
  "ingress_bytes": 50000,
  "concurrent_requests": 2
}
```

**Error Responses:**

| Status | Reason |
|--------|--------|
| 404 | No usage data for this app |

**Example:**

```bash
curl http://localhost:3000/_apps/my-app/usage
```

---

### DELETE /_apps/:id

Manually evict an app's V8 isolate and clear its metering data. The next request to this app will create a fresh isolate.

**Success Response (200):**

```json
{
  "evicted": "my-app"
}
```

**Example:**

```bash
curl -X DELETE http://localhost:3000/_apps/my-app
```
