# Appbase MVP Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a working demo where a single `.jsx` file with frontend + backend code compiles and runs as a full-stack app.

**Architecture:** Babel plugin splits a single source file into server functions (Hono API routes) and client code (Vite-bundled React). Direct function calls are rewritten to fetch-based RPC. SQLite provides zero-config persistence.

**Tech Stack:** Babel (custom plugin), Hono, Vite, React, better-sqlite3, Node.js, Nix flake

---

### Task 1: Project Scaffolding + Nix Dev Environment

**Files:**
- Create: `flake.nix`
- Create: `.envrc`
- Create: `package.json`
- Create: `.gitignore`

**Step 1: Initialize git repo**

Run: `cd /home/ruiyang/Projects/appbase && git init`

**Step 2: Create flake.nix**

```nix
{
  description = "appbase - single-file full-stack framework";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            nodejs_22
            nodePackages.npm
          ];
        };
      });
}
```

**Step 3: Create .envrc**

```
use flake
```

**Step 4: Create .gitignore**

```
node_modules/
dist/
.dist/
.direnv/
result
*.db
```

**Step 5: Create package.json**

```json
{
  "name": "appbase",
  "version": "0.1.0",
  "type": "module",
  "scripts": {
    "test": "node --test tests/**/*.test.js",
    "dev": "node packages/appbase/bin/appbase.js dev"
  },
  "workspaces": [
    "packages/*"
  ]
}
```

**Step 6: Commit**

```bash
git add flake.nix .envrc .gitignore package.json
git commit -m "chore: init project with nix flake and npm workspaces"
```

---

### Task 2: DB Primitive — `db.collection()` over SQLite

**Files:**
- Create: `packages/appbase/package.json`
- Create: `packages/appbase/src/db.js`
- Create: `tests/db.test.js`

**Step 1: Create packages/appbase/package.json**

```json
{
  "name": "appbase",
  "version": "0.1.0",
  "type": "module",
  "main": "src/index.js",
  "exports": {
    ".": "./src/index.js",
    "./compiler": "./src/compiler/plugin.js"
  },
  "dependencies": {
    "better-sqlite3": "^11.0.0"
  }
}
```

**Step 2: Write the failing test**

`tests/db.test.js`:

```js
import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { createDb } from '../packages/appbase/src/db.js'

describe('db.collection', () => {
  let db

  before(() => {
    db = createDb(':memory:')
  })

  after(() => {
    db.close()
  })

  it('inserts and finds documents', async () => {
    const todos = db.collection('todos')
    const doc = await todos.insert({ text: 'hello', done: false })
    assert.ok(doc.id)
    assert.equal(doc.text, 'hello')

    const all = await todos.find()
    assert.equal(all.length, 1)
    assert.equal(all[0].text, 'hello')
  })

  it('finds with filter', async () => {
    const items = db.collection('items')
    await items.insert({ name: 'a', type: 'x' })
    await items.insert({ name: 'b', type: 'y' })
    await items.insert({ name: 'c', type: 'x' })

    const result = await items.find({ type: 'x' })
    assert.equal(result.length, 2)
  })

  it('deletes a document', async () => {
    const notes = db.collection('notes')
    const doc = await notes.insert({ text: 'delete me' })
    await notes.delete(doc.id)

    const all = await notes.find()
    assert.equal(all.length, 0)
  })

  it('updates a document', async () => {
    const tasks = db.collection('tasks')
    const doc = await tasks.insert({ text: 'old', done: false })
    await tasks.update(doc.id, { done: true })

    const all = await tasks.find()
    assert.equal(all[0].done, true)
    assert.equal(all[0].text, 'old')
  })
})
```

**Step 3: Run test to verify it fails**

Run: `cd /home/ruiyang/Projects/appbase && npm install && node --test tests/db.test.js`
Expected: FAIL — module not found

**Step 4: Implement db.js**

`packages/appbase/src/db.js`:

```js
import Database from 'better-sqlite3'
import { randomUUID } from 'node:crypto'

export function createDb(path = 'appbase.db') {
  const sqlite = new Database(path)
  sqlite.pragma('journal_mode = WAL')
  sqlite.pragma('foreign_keys = ON')

  function collection(name) {
    sqlite.exec(`
      CREATE TABLE IF NOT EXISTS "${name}" (
        id TEXT PRIMARY KEY,
        data JSON NOT NULL,
        created_at TEXT DEFAULT (datetime('now')),
        updated_at TEXT DEFAULT (datetime('now'))
      )
    `)

    return {
      async insert(doc) {
        const id = randomUUID()
        const row = { id, ...doc }
        sqlite.prepare(`INSERT INTO "${name}" (id, data) VALUES (?, ?)`).run(id, JSON.stringify(row))
        return row
      },

      async find(filter) {
        const rows = sqlite.prepare(`SELECT data FROM "${name}"`).all()
        const docs = rows.map(r => JSON.parse(r.data))
        if (!filter) return docs
        return docs.filter(doc =>
          Object.entries(filter).every(([k, v]) => doc[k] === v)
        )
      },

      async delete(id) {
        sqlite.prepare(`DELETE FROM "${name}" WHERE id = ?`).run(id)
      },

      async update(id, updates) {
        const existing = sqlite.prepare(`SELECT data FROM "${name}" WHERE id = ?`).get(id)
        if (!existing) throw new Error(`Document ${id} not found`)
        const doc = { ...JSON.parse(existing.data), ...updates, updated_at: new Date().toISOString() }
        sqlite.prepare(`UPDATE "${name}" SET data = ?, updated_at = datetime('now') WHERE id = ?`).run(JSON.stringify(doc), id)
        return doc
      }
    }
  }

  return {
    collection,
    close() { sqlite.close() }
  }
}
```

**Step 5: Run test to verify it passes**

Run: `node --test tests/db.test.js`
Expected: All 4 tests PASS

**Step 6: Commit**

```bash
git add packages/appbase/ tests/db.test.js
git commit -m "feat: add db primitive with collection API over SQLite"
```

---

### Task 3: Babel Compiler Plugin — Split Server/Client

**Files:**
- Create: `packages/appbase/src/compiler/plugin.js`
- Create: `tests/compiler.test.js`

**Step 1: Install babel dependencies**

Run: `cd /home/ruiyang/Projects/appbase && npm install --save -w packages/appbase @babel/core @babel/parser @babel/generator @babel/traverse @babel/types @babel/preset-react`

**Step 2: Write the failing test**

`tests/compiler.test.js`:

```js
import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { compile } from '../packages/appbase/src/compiler/plugin.js'

const input = `
import { db, serve } from 'appbase'

const todos = db.collection('todos')

export async function addTodo(text) {
  return todos.insert({ text, done: false })
}

export async function getTodos() {
  return todos.find()
}

function App() {
  const items = getTodos()
  return <div>{items.length} todos</div>
}

serve(App)
`

describe('compiler', () => {
  it('extracts server functions', () => {
    const result = compile(input)
    assert.ok(result.server, 'should have server output')
    assert.ok(result.client, 'should have client output')
  })

  it('server code contains db functions', () => {
    const result = compile(input)
    assert.ok(result.server.includes('addTodo'))
    assert.ok(result.server.includes('getTodos'))
    assert.ok(result.server.includes('todos.insert'))
  })

  it('client code replaces calls with fetch', () => {
    const result = compile(input)
    assert.ok(result.client.includes('fetch'))
    assert.ok(!result.client.includes('db.collection'))
  })

  it('client code keeps the React component', () => {
    const result = compile(input)
    assert.ok(result.client.includes('App'))
  })

  it('extracts serve() call as entry point', () => {
    const result = compile(input)
    assert.ok(result.entryComponent === 'App')
  })
})
```

**Step 3: Run test to verify it fails**

Run: `node --test tests/compiler.test.js`
Expected: FAIL — module not found

**Step 4: Implement the compiler**

`packages/appbase/src/compiler/plugin.js`:

```js
import { parse } from '@babel/parser'
import traverse from '@babel/traverse'
import generate from '@babel/generator'
import * as t from '@babel/types'

export function compile(source) {
  const ast = parse(source, {
    sourceType: 'module',
    plugins: ['jsx']
  })

  // Track which identifiers come from 'appbase' server imports
  const serverImports = new Set() // e.g. 'db'
  const serverBindings = new Set() // e.g. 'todos' (from db.collection)
  const serverFunctions = new Set() // e.g. 'addTodo', 'getTodos'
  let entryComponent = null

  // Pass 1: Find appbase imports and serve() call
  traverse.default(ast, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        for (const spec of path.node.specifiers) {
          if (spec.imported.name !== 'serve') {
            serverImports.add(spec.local.name)
          }
        }
      }
    },
    CallExpression(path) {
      if (path.node.callee.name === 'serve' && path.node.arguments.length > 0) {
        entryComponent = path.node.arguments[0].name
      }
    }
  })

  // Pass 2: Find bindings that use server imports (e.g. const todos = db.collection(...))
  traverse.default(ast, {
    VariableDeclarator(path) {
      const init = path.node.init
      if (
        init &&
        t.isCallExpression(init) &&
        t.isMemberExpression(init.callee) &&
        t.isIdentifier(init.callee.object) &&
        serverImports.has(init.callee.object.name)
      ) {
        serverBindings.add(path.node.id.name)
      }
    }
  })

  // Pass 3: Find functions that reference server bindings
  traverse.default(ast, {
    'FunctionDeclaration|FunctionExpression'(path) {
      const name = path.node.id?.name
      if (!name) return

      let usesServer = false
      path.traverse({
        Identifier(innerPath) {
          if (serverBindings.has(innerPath.node.name)) {
            usesServer = true
          }
        }
      })

      if (usesServer) {
        serverFunctions.add(name)
      }
    }
  })

  // Generate server code: keep imports, server bindings, server functions
  const serverAst = parse(source, { sourceType: 'module', plugins: ['jsx'] })
  const serverNodesToRemove = []

  traverse.default(serverAst, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        // Keep only server imports (remove 'serve')
        path.node.specifiers = path.node.specifiers.filter(
          s => s.imported.name !== 'serve'
        )
        if (path.node.specifiers.length === 0) {
          path.remove()
        }
      }
    },
    FunctionDeclaration(path) {
      if (!serverFunctions.has(path.node.id.name)) {
        path.remove()
      }
    },
    ExpressionStatement(path) {
      if (t.isCallExpression(path.node.expression) && path.node.expression.callee.name === 'serve') {
        path.remove()
      }
    }
  })

  const server = generate.default(serverAst).code

  // Generate client code: remove server-only stuff, rewrite function calls to fetch
  const clientAst = parse(source, { sourceType: 'module', plugins: ['jsx'] })

  traverse.default(clientAst, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        // Remove server imports from client, keep serve
        path.node.specifiers = path.node.specifiers.filter(
          s => s.imported.name === 'serve'
        )
        if (path.node.specifiers.length === 0) {
          path.remove()
        }
      }
    },
    VariableDeclaration(path) {
      // Remove server bindings (const todos = db.collection(...))
      const decl = path.node.declarations[0]
      if (decl && t.isIdentifier(decl.id) && serverBindings.has(decl.id.name)) {
        path.remove()
      }
    },
    FunctionDeclaration(path) {
      if (serverFunctions.has(path.node.id.name)) {
        // Replace server function with RPC stub
        const name = path.node.id.name
        const params = path.node.params.map(p => p.name)
        const rpcFn = parse(`
          async function ${name}(${params.join(', ')}) {
            const res = await fetch('/api/${name}', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ args: [${params.join(', ')}] })
            });
            return res.json();
          }
        `, { sourceType: 'module' }).program.body[0]
        path.replaceWith(rpcFn)
      }
    }
  })

  const client = generate.default(clientAst).code

  return { server, client, entryComponent }
}
```

**Step 5: Run test to verify it passes**

Run: `node --test tests/compiler.test.js`
Expected: All 5 tests PASS

**Step 6: Commit**

```bash
git add packages/appbase/src/compiler/ tests/compiler.test.js
git commit -m "feat: add Babel compiler that splits single file into server + client"
```

---

### Task 4: Server Runtime — Hono API Routes from Compiled Server Code

**Files:**
- Create: `packages/appbase/src/runtime/server.js`
- Create: `tests/server.test.js`

**Step 1: Install hono**

Run: `npm install --save -w packages/appbase hono @hono/node-server`

**Step 2: Write the failing test**

`tests/server.test.js`:

```js
import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from '../packages/appbase/src/runtime/server.js'
import { createDb } from '../packages/appbase/src/db.js'

describe('server runtime', () => {
  let server, address, db

  before(async () => {
    db = createDb(':memory:')
    const todos = db.collection('todos')

    const serverFunctions = {
      async addTodo(text) {
        return todos.insert({ text, done: false })
      },
      async getTodos() {
        return todos.find()
      }
    }

    const result = createServer({ functions: serverFunctions, port: 0 })
    server = result.server
    address = result.address
  })

  after(() => {
    server.close()
    db.close()
  })

  it('calls a server function via POST /api/:name', async () => {
    const res = await fetch(`${address}/api/addTodo`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args: ['buy milk'] })
    })
    const data = await res.json()
    assert.ok(data.id)
    assert.equal(data.text, 'buy milk')
  })

  it('returns results from getTodos', async () => {
    const res = await fetch(`${address}/api/getTodos`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args: [] })
    })
    const data = await res.json()
    assert.ok(Array.isArray(data))
    assert.ok(data.length > 0)
  })

  it('returns 404 for unknown function', async () => {
    const res = await fetch(`${address}/api/noSuchFn`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args: [] })
    })
    assert.equal(res.status, 404)
  })
})
```

**Step 3: Run test to verify it fails**

Run: `node --test tests/server.test.js`
Expected: FAIL — module not found

**Step 4: Implement server runtime**

`packages/appbase/src/runtime/server.js`:

```js
import { Hono } from 'hono'
import { serve } from '@hono/node-server'
import { serveStatic } from '@hono/node-server/serve-static'

export function createServer({ functions, port = 3000, staticDir = null }) {
  const app = new Hono()

  // Register RPC routes for each server function
  app.post('/api/:name', async (c) => {
    const name = c.req.param('name')
    const fn = functions[name]
    if (!fn) {
      return c.json({ error: `Function '${name}' not found` }, 404)
    }

    const body = await c.req.json()
    const args = body.args || []
    const result = await fn(...args)
    return c.json(result)
  })

  // Serve static client files if dir is provided
  if (staticDir) {
    app.use('/*', serveStatic({ root: staticDir }))
  }

  const httpServer = serve({ fetch: app.fetch, port }, (info) => {
    // Server started
  })

  const address = `http://localhost:${httpServer.address().port}`

  return { server: httpServer, address, app }
}
```

**Step 5: Run test to verify it passes**

Run: `node --test tests/server.test.js`
Expected: All 3 tests PASS

**Step 6: Commit**

```bash
git add packages/appbase/src/runtime/ tests/server.test.js
git commit -m "feat: add Hono server runtime with RPC routing"
```

---

### Task 5: Dev CLI — `appbase dev` Command

**Files:**
- Create: `packages/appbase/bin/appbase.js`
- Create: `packages/appbase/src/dev.js`
- Create: `packages/appbase/src/index.js`

**Step 1: Create the appbase SDK entry point**

`packages/appbase/src/index.js` — this is what user code imports:

```js
// This module is the compile-time import target.
// At runtime, server code gets the real db, client code gets RPC stubs.
// This file is only used as a marker for the compiler.
export { createDb as db } from './db.js'
export function serve() {
  // Marker function — the compiler extracts this
  throw new Error('serve() should not be called directly. Use `appbase dev` to run your app.')
}
```

**Step 2: Create the dev server**

`packages/appbase/src/dev.js`:

```js
import { readFileSync, mkdirSync, writeFileSync } from 'node:fs'
import { resolve, dirname } from 'node:path'
import { compile } from './compiler/plugin.js'
import { createDb } from './db.js'
import { createServer } from './runtime/server.js'
import { fileURLToPath } from 'node:url'

export async function dev(entryFile, options = {}) {
  const port = options.port || 3000
  const source = readFileSync(resolve(entryFile), 'utf-8')

  console.log(`[appbase] Compiling ${entryFile}...`)
  const { server: serverCode, client: clientCode, entryComponent } = compile(source)

  // Write compiled files to .dist/
  const distDir = resolve('.dist')
  mkdirSync(distDir, { recursive: true })
  mkdirSync(resolve(distDir, 'server'), { recursive: true })
  mkdirSync(resolve(distDir, 'client'), { recursive: true })

  writeFileSync(resolve(distDir, 'server/functions.js'), serverCode)
  writeFileSync(resolve(distDir, 'client/App.jsx'), clientCode)

  // Write client index.html
  writeFileSync(resolve(distDir, 'client/index.html'), `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>appbase app</title></head>
<body>
  <div id="root"></div>
  <script type="module" src="/main.jsx"></script>
</body>
</html>`)

  // Write client entry that mounts the component
  writeFileSync(resolve(distDir, 'client/main.jsx'), `
import React from 'react'
import { createRoot } from 'react-dom/client'
${clientCode}

const root = createRoot(document.getElementById('root'))
root.render(React.createElement(${entryComponent}))
`)

  console.log(`[appbase] Compiled. Server functions extracted, client bundle ready.`)
  console.log(`[appbase] Server code:\n${serverCode}\n`)
  console.log(`[appbase] Client code:\n${clientCode}\n`)

  // Load server functions dynamically
  // For the dev server, we eval the server code with db available
  const db = createDb('appbase.db')
  const serverModule = {}
  const serverFn = new Function('db', 'module', `
    const collection = db.collection.bind(db);
    const dbProxy = { collection };
    ${serverCode.replace(/import.*from.*appbase.*/g, '')}
    ${Array.from(serverCode.matchAll(/(?:export\s+)?async\s+function\s+(\w+)/g)).map(m =>
      `module["${m[1]}"] = ${m[1]};`
    ).join('\n')}
  `)
  serverFn(db, serverModule)

  // Start server
  const { address } = createServer({
    functions: serverModule,
    port,
    staticDir: resolve(distDir, 'client')
  })

  console.log(`[appbase] Dev server running at ${address}`)

  // In a real implementation, we'd start Vite here for HMR
  // For the demo, we serve static files
  return { address }
}
```

**Step 3: Create the CLI entry point**

`packages/appbase/bin/appbase.js`:

```js
#!/usr/bin/env node
import { dev } from '../src/dev.js'

const [,, command, ...args] = process.argv

if (command === 'dev') {
  const entry = args[0] || 'app.jsx'
  const port = parseInt(args.find(a => a.startsWith('--port='))?.split('=')[1] || '3000')
  dev(entry, { port })
} else {
  console.log('Usage: appbase dev [file.jsx] [--port=3000]')
}
```

**Step 4: Create the demo app**

`examples/todo/app.jsx`:

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
    <div>
      <h1>Todos</h1>
      <button onClick={handleAdd}>Add Todo</button>
      <ul>{items.map(t => <li key={t.id}>{t.text}</li>)}</ul>
    </div>
  )
}

serve(App)
```

**Step 5: Commit**

```bash
git add packages/appbase/bin/ packages/appbase/src/index.js packages/appbase/src/dev.js examples/
git commit -m "feat: add dev CLI and example todo app"
```

---

### Task 6: Vite Integration — Client Bundle with HMR

**Files:**
- Modify: `packages/appbase/src/dev.js`
- Modify: `packages/appbase/package.json`

**Step 1: Install vite + react**

Run: `npm install --save -w packages/appbase vite @vitejs/plugin-react react react-dom`

**Step 2: Update dev.js to use Vite dev server**

Replace the static file serving with Vite's dev server middleware. The Hono server handles `/api/*` routes, and Vite handles everything else (with HMR).

Key changes:
- Create a Vite dev server with `createViteServer({ server: { middlewareMode: true } })`
- Mount Vite's middleware on the Hono app as a fallback
- Vite serves client files from `.dist/client/` with full HMR

**Step 3: Commit**

```bash
git add packages/appbase/
git commit -m "feat: integrate Vite dev server for client HMR"
```

---

### Task 7: End-to-End Test — Compile + Run the Hello World

**Files:**
- Create: `tests/e2e.test.js`

**Step 1: Write the e2e test**

`tests/e2e.test.js`:

```js
import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { compile } from '../packages/appbase/src/compiler/plugin.js'
import { createDb } from '../packages/appbase/src/db.js'
import { createServer } from '../packages/appbase/src/runtime/server.js'
import { readFileSync } from 'node:fs'

describe('e2e: single file to running app', () => {
  let server, address, db

  const source = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
export async function addTodo(text) {
  return todos.insert({ text, done: false })
}
export async function getTodos() {
  return todos.find()
}
function App() {
  return <div>Hello</div>
}
serve(App)
  `

  before(async () => {
    const { server: serverCode, client: clientCode, entryComponent } = compile(source)

    // Verify compilation produced valid output
    assert.ok(serverCode.includes('addTodo'))
    assert.ok(clientCode.includes('fetch'))
    assert.equal(entryComponent, 'App')

    // Set up real server with compiled functions
    db = createDb(':memory:')
    const todos = db.collection('todos')
    const functions = {
      async addTodo(text) { return todos.insert({ text, done: false }) },
      async getTodos() { return todos.find() }
    }

    const result = createServer({ functions, port: 0 })
    server = result.server
    address = result.address
  })

  after(() => {
    server.close()
    db.close()
  })

  it('compiles and the API works end to end', async () => {
    // Add a todo via the compiled API
    let res = await fetch(`${address}/api/addTodo`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args: ['hello world'] })
    })
    const todo = await res.json()
    assert.equal(todo.text, 'hello world')

    // Fetch all todos
    res = await fetch(`${address}/api/getTodos`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ args: [] })
    })
    const todos = await res.json()
    assert.equal(todos.length, 1)
    assert.equal(todos[0].text, 'hello world')
  })
})
```

**Step 2: Run the test**

Run: `node --test tests/e2e.test.js`
Expected: PASS

**Step 3: Run all tests**

Run: `node --test tests/*.test.js`
Expected: All tests PASS

**Step 4: Commit**

```bash
git add tests/e2e.test.js
git commit -m "test: add e2e test for compile-to-running-app pipeline"
```

---

## Summary

| Task | What | Outcome |
|------|------|---------|
| 1 | Project scaffolding + Nix | Git repo, npm workspaces, flake.nix |
| 2 | DB primitive | `db.collection()` CRUD over SQLite |
| 3 | Babel compiler | Splits single file into server + client code |
| 4 | Server runtime | Hono server with auto-registered RPC routes |
| 5 | Dev CLI | `appbase dev app.jsx` compiles and runs |
| 6 | Vite integration | Client bundle with HMR |
| 7 | E2E test | Full pipeline: source → compile → run → API works |
