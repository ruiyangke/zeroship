# Multi-File App Convention Spec

## Overview

Appbase supports convention-based file-system routing (Next.js App Router style). Each directory under `app/` can define routes, layouts, loading states, error boundaries, and colocated server modules.

---

## 1. Directory Structure

```
app/
  layout.jsx            → root layout (wraps everything)
  page.jsx              → / route
  loading.jsx           → root loading state
  error.jsx             → root error boundary
  not-found.jsx         → 404 page
  template.jsx          → like layout but re-mounts on navigation

  about/
    page.jsx            → /about

  todos/
    layout.jsx          → /todos layout (wraps todos pages)
    page.jsx            → /todos
    loading.jsx         → /todos loading state
    error.jsx           → /todos error boundary
    server.js           → "use server" module for /todos
    [id]/
      page.jsx          → /todos/:id
      server.js         → "use server" module for /todos/:id

  settings/
    page.jsx            → /settings
    profile/
      page.jsx          → /settings/profile

  components/           → shared components (not routes)
    TodoItem.jsx
    Header.jsx

  lib/                  → shared utilities (not routes)
    utils.js
    constants.js
```

---

## 2. Special Files

| File | Purpose | Export | Required? |
|------|---------|--------|-----------|
| `page.jsx` | Route UI component | `export default function Page()` | Yes (makes a directory a route) |
| `layout.jsx` | Wraps child routes, persists across navigation | `export default function Layout({ children })` | No (inherits parent layout) |
| `loading.jsx` | Suspense fallback while page loads | `export default function Loading()` | No |
| `error.jsx` | Error boundary for this route segment | `export default function Error({ error, reset })` | No |
| `not-found.jsx` | 404 for this route segment | `export default function NotFound()` | No |
| `template.jsx` | Like layout but re-mounts on every navigation | `export default function Template({ children })` | No |
| `server.js` | Colocated "use server" module | Named exports of server functions | No |

### Rules

- A directory becomes a route only if it contains `page.jsx`
- `layout.jsx` wraps all child routes including nested ones
- `template.jsx` is like layout but creates a new instance on each navigation
- `loading.jsx` is shown while the page component is being loaded/suspended
- `error.jsx` catches errors in the page and its children
- `server.js` has an implicit `"use server"` — all exports are server functions
- `components/`, `lib/`, and any directory without `page.jsx` are not routes

---

## 3. Server Code

### Colocated server.js

Files named `server.js` (or `server.ts`) are implicitly `"use server"` modules. All their exports become server functions (RPC endpoints).

```jsx
// app/todos/server.js — implicitly "use server"
import { db } from 'appbase'
const todos = db.collection('todos')

export async function getTodos() {
  return todos.find()
}

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}
```

### Importing server functions in pages

```jsx
// app/todos/page.jsx
import { getTodos, addTodo } from './server'

export default function TodosPage() {
  const [items, setItems] = useState([])
  useEffect(() => { getTodos().then(setItems) }, [])
  // ...
}
```

The compiler resolves `./server` imports and replaces them with RPC stubs in the client output.

### Inline "use server" in pages

Pages can also define server functions inline:

```jsx
// app/todos/page.jsx
import { db } from 'appbase'
const todos = db.collection('todos')

export async function deleteTodo(id) {
  "use server"
  return todos.delete(id)
}

export default function TodosPage() {
  // deleteTodo is available as an RPC call
}
```

Both approaches (colocated server.js and inline "use server") can be used in the same app.

---

## 4. Route Resolution

### Path mapping

| File path | URL route | Dynamic? |
|-----------|-----------|----------|
| `app/page.jsx` | `/` | No |
| `app/about/page.jsx` | `/about` | No |
| `app/todos/page.jsx` | `/todos` | No |
| `app/todos/[id]/page.jsx` | `/todos/:id` | Yes |
| `app/blog/[...slug]/page.jsx` | `/blog/*` | Catch-all |
| `app/(group)/settings/page.jsx` | `/settings` | No (group ignored in URL) |

### Dynamic segments

- `[param]` — single dynamic segment: `/todos/:id`
- `[...param]` — catch-all: `/blog/*`
- `(group)` — route group (no URL segment, just for organization)

### Layout nesting

```
Request: /todos/abc123

Renders:
  app/layout.jsx
    └── app/todos/layout.jsx
          └── app/todos/[id]/page.jsx  (params: { id: 'abc123' })
```

Each layout wraps its children. The root layout is always rendered.

---

## 5. Compiler Changes

### Current: single file

```js
compile(source, { target: 'node' })
// → { server, client, entryComponent, serverFunctions }
```

### New: directory mode

```js
compile('app/', { target: 'node' })
// → {
//   routes: [
//     {
//       path: '/',
//       page: { client: '...', server: '...' },
//       layout: { client: '...' },
//       loading: { client: '...' },
//       error: { client: '...' },
//       serverFunctions: ['...']
//     },
//     ...
//   ],
//   rootLayout: '...',
//   serverBundle: '...',      // all server functions bundled
//   clientBundle: '...',      // all client code bundled
//   serverFunctions: ['...'], // all RPC endpoint names
// }
```

### Single file mode is preserved

The single-file `compile(source, { target })` still works for the hello-world / demo use case. Directory mode is additive.

---

## 6. Runtime Changes

### Client-side routing

The client bundle includes a router that:
1. Matches URL to route definitions
2. Renders the layout hierarchy
3. Shows loading states during navigation
4. Catches errors with error boundaries
5. Handles dynamic params

### Server-side

All server functions from all routes are registered in a single JSON-RPC endpoint. Function names are globally unique (namespace by route if needed).

```
POST /rpc
{ "method": "getTodos", "params": [], "id": 1 }
```

---

## 7. Migration from Single-File

Single-file apps (`app.jsx` with `serve(App)`) continue to work. To migrate:

```
# Before
app.jsx

# After
app/
  page.jsx      ← move App component here
  server.js     ← move server functions here
  layout.jsx    ← optional root layout
```

The `serve()` function is replaced by the convention — `app/layout.jsx` is the root, `app/page.jsx` is the index route.
