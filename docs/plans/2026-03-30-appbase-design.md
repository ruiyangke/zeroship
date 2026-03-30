# Appbase: Single-File Full-Stack Framework

## Vision

A BaaS infrastructure where AI-generated apps are written as single `.jsx` files containing both frontend and backend code. A Babel-based compiler splits them into client + server, auto-generating the RPC bridge.

## Hello World

```jsx
import { db, serve } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}

function App() {
  const [items, setItems] = useState([])
  useEffect(() => { getTodos().then(setItems) }, [])
  const handleAdd = async (text) => {
    await addTodo(text)
    setItems(await getTodos())
  }
  return (
    <div>
      <h1>Todos</h1>
      <button onClick={() => handleAdd('New todo')}>Add</button>
      <ul>{items.map(t => <li>{t.text}</li>)}</ul>
    </div>
  )
}

serve(App)
```

## Architecture

### Compiler (Babel Transform)

1. Parse the single file
2. Identify server functions: any function that references `db`, `auth`, `storage` imports
3. Extract server functions into a separate server module
4. Rewrite client-side calls to those functions into `fetch('/api/<functionName>', ...)` RPC calls
5. Bundle client code with Vite (React/Preact)

### Runtime

- **Server:** Hono (lightweight, fast) — serves API routes + static client bundle
- **Client:** Vite-bundled React app
- **DB:** SQLite via better-sqlite3 (embedded, zero config)
- **Dev server:** `npx appbase dev` — watches file, recompiles, hot reloads

### Built-in Primitives (v1)

- `db` — collection-based API over SQLite (schemaless, document-style)
- `serve` — marks the root component and starts the app

### Future Primitives

- `auth` — user accounts, sessions, permissions
- `storage` — file/object storage
- `realtime` — websocket subscriptions

## Tech Stack

- **Compiler:** Babel with custom plugin
- **Server:** Hono on Node.js
- **Client bundler:** Vite + React
- **Database:** SQLite (better-sqlite3)
- **Dev environment:** Nix flake

## Key Design Decisions

- Babel for demo, migrate to SWC/custom compiler later
- SQLite embedded for zero-config, pluggable for production
- Explicit imports (`from 'appbase'`) over magic globals
- Server/client boundary inferred by usage of server primitives
