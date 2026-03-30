# Appbase Compiler Spec v1

## Overview

The appbase compiler takes a single `.jsx` file containing both frontend and backend code, and splits it into two outputs: a **server bundle** (runs on Node or Rust/deno_core) and a **client bundle** (runs in the browser).

---

## 1. Server Detection Rules

A function is a **server function** if ANY of these is true:

1. **Explicit directive** — the function body contains `"use server"`
2. **Direct tainted reference** — the function directly references a tainted binding
3. **File-level directive** — the file has `"use server"` at the top AND the function is exported

### Taint Propagation

A binding is **tainted** if:

- It is imported from a module that has `"use server"` at its top level
- It is initialized from a tainted binding (direct, level 1 only)

```
"use server" module → imported binding (tainted)
                    → variable initialized from it (tainted)
                    → function using either → SERVER
```

Taint is **direct only** — no transitive call graph analysis. If `fnA` calls `fnB` which uses `db`, only `fnB` is a server function. `fnA` is client code that calls `fnB` via RPC.

### Examples

```jsx
import { db } from 'appbase'           // db is tainted (appbase/server has "use server")

const todos = db.collection('todos')    // tainted (initialized from db)

// SERVER — directly references tainted binding `todos`
async function getTodos() {
  return todos.find()
}

// SERVER — has "use server" directive
async function getStats() {
  "use server"
  return computeExpensiveStats()
}

// CLIENT — no tainted references, no directive
function formatTodo(todo) {
  return todo.text.toUpperCase()
}

// CLIENT — calls server function but doesn't directly reference tainted binding
async function getActiveTodos() {
  const all = await getTodos()     // this becomes an RPC call
  return all.filter(t => !t.done)
}

// CLIENT — React component, no tainted references
function App() {
  return <div>Hello</div>
}
```

---

## 2. Server Output

### What is included

- Server functions (per Section 1 rules)
- Tainted bindings they depend on (e.g. `const todos = db.collection('todos')`)
- Server-side imports from `"use server"` modules

### What is excluded

- All client code (components, styles, non-server variables, helpers)
- `serve()` call
- Non-server imports
- `"use server"` directives (stripped after processing)

### Target: Rust (deno_core)

No imports. Server primitives (`db`) are injected as globals by the runtime. Functions registered via `globalThis.__rpc`.

```js
const todos = db.collection('todos')

async function addTodo(text) {
  return todos.insert({ text, done: false })
}

async function getTodos() {
  return todos.find()
}

globalThis.__rpc = { addTodo, getTodos }
```

### Target: Node

Imports kept (rewritten to real SDK paths). Functions exported as ES modules.

```js
import { db } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}
```

---

## 3. Client Output

### What is included

- All non-server functions (components, helpers, event handlers)
- All non-tainted variables (styles, constants, config)
- Server functions replaced with JSON-RPC 2.0 stubs
- Non-appbase imports (React, third-party, etc.)

### What is excluded

- `import { ... } from 'appbase'` — all appbase imports removed
- `serve(App)` call — removed
- Tainted bindings (e.g. `const todos = db.collection(...)`)
- `"use server"` directives (stripped)

### RPC Stub Generation

Each server function is replaced with a fetch-based JSON-RPC 2.0 call:

```js
async function addTodo(text) {
  const res = await fetch('/rpc', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      method: 'addTodo',
      params: [text],
      id: Date.now()
    })
  });
  const data = await res.json();
  if (data.error) throw new Error(data.error.message);
  return data.result;
}
```

### Client output is the same regardless of server target (Rust or Node).

---

## 4. `serve()` and Entry Point

`serve(Component)` declares the root component for the app. The compiler:

1. Extracts the component name as `entryComponent`
2. Strips the `serve()` call from both server and client output

`serve()` is NOT a server marker. It is purely a client-side concept — "this is the root component to render."

The dev server uses `entryComponent` to generate the mount code:

```js
const root = createRoot(document.getElementById('root'))
root.render(React.createElement(App))
```

---

## 5. Compiler API

```js
import { compile } from 'appbase/compiler'

const { server, client, entryComponent, serverFunctions } = compile(source, {
  target: 'rust' | 'node'  // default: 'node'
})
```

### Return value

| Field | Type | Description |
|-------|------|-------------|
| `server` | `string` | Compiled server code |
| `client` | `string` | Compiled client code with RPC stubs |
| `entryComponent` | `string` | Name of the root component from `serve()` |
| `serverFunctions` | `string[]` | List of detected server function names |

---

## 6. Full Example

### Input

```jsx
import { db, serve } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}

export async function toggleTodo(id, done) {
  return todos.update(id, { done })
}

export async function deleteTodo(id) {
  return todos.delete(id)
}

function App() {
  const [items, setItems] = React.useState([])

  React.useEffect(() => {
    getTodos().then(setItems)
  }, [])

  const handleAdd = async () => {
    const text = prompt('Todo text:')
    if (text) {
      await addTodo(text)
      setItems(await getTodos())
    }
  }

  return (
    <div style={styles.wrapper}>
      <h1>Todos</h1>
      <button onClick={handleAdd}>Add</button>
      <ul>{items.map(t => <li key={t.id}>{t.text}</li>)}</ul>
    </div>
  )
}

const styles = {
  wrapper: { padding: '20px', fontFamily: 'sans-serif' }
}

serve(App)
```

### Server output (Rust target)

```js
const todos = db.collection('todos')

async function addTodo(text) {
  return todos.insert({ text, done: false })
}

async function getTodos() {
  return todos.find()
}

async function toggleTodo(id, done) {
  return todos.update(id, { done })
}

async function deleteTodo(id) {
  return todos.delete(id)
}

globalThis.__rpc = { addTodo, getTodos, toggleTodo, deleteTodo }
```

### Client output

```jsx
async function addTodo(text) {
  const res = await fetch('/rpc', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', method: 'addTodo', params: [text], id: Date.now() })
  });
  const data = await res.json();
  if (data.error) throw new Error(data.error.message);
  return data.result;
}

async function getTodos() { /* same RPC stub pattern */ }
async function toggleTodo(id, done) { /* same */ }
async function deleteTodo(id) { /* same */ }

function App() {
  const [items, setItems] = React.useState([])

  React.useEffect(() => {
    getTodos().then(setItems)
  }, [])

  const handleAdd = async () => {
    const text = prompt('Todo text:')
    if (text) {
      await addTodo(text)
      setItems(await getTodos())
    }
  }

  return (
    <div style={styles.wrapper}>
      <h1>Todos</h1>
      <button onClick={handleAdd}>Add</button>
      <ul>{items.map(t => <li key={t.id}>{t.text}</li>)}</ul>
    </div>
  )
}

const styles = {
  wrapper: { padding: '20px', fontFamily: 'sans-serif' }
}
```

### Metadata

```json
{
  "entryComponent": "App",
  "serverFunctions": ["addTodo", "getTodos", "toggleTodo", "deleteTodo"]
}
```

---

## 7. Edge Cases

### Function with `"use server"` but no tainted references

```jsx
async function getTime() {
  "use server"
  return Date.now()  // runs on server, no db needed
}
```

→ Server function. Useful for logic that should run server-side (e.g. secrets, env vars).

### Variable that uses a tainted binding but isn't a function

```jsx
const count = db.collection('counts')  // tainted binding
const defaultCount = 0                 // NOT tainted — plain value
```

`count` is extracted to server. `defaultCount` stays in client.

### Non-exported server function

```jsx
async function internalHelper() {
  return todos.find({ active: true })
}

export async function getActive() {
  "use server"
  return internalHelper()
}
```

Both are server functions: `internalHelper` by taint, `getActive` by directive. Both go to server output. Only `getActive` becomes an RPC endpoint (it's exported). `internalHelper` is a server-side helper, not callable from client.

### Third-party `"use server"` modules

```jsx
import { charge } from 'appbase-stripe'  // module has "use server"

export async function pay(amount) {
  return charge(amount)  // tainted — charge comes from "use server" module
}
```

→ `pay` is a server function. The compiler doesn't need to know about Stripe — just that the import source has `"use server"`.
