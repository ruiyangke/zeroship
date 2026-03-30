# Compiler Rewrite Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Rewrite the appbase compiler to match the v1 spec — `"use server"` directives, taint propagation, clean server/client split, TypeScript support, and target-specific output (Rust/Node).

**Architecture:** Complete rewrite of `packages/appbase/src/compiler/plugin.js`. Four-pass Babel transform: (1) collect taint sources, (2) propagate taint to bindings, (3) detect server functions, (4) generate server + client output. Add TypeScript parser plugin. Add `target` option.

**Tech Stack:** Babel (@babel/parser, @babel/traverse, @babel/generator, @babel/types), Node.js test runner

---

### Task 1: Write Comprehensive Tests First

**Files:**
- Modify: `tests/compiler.test.js`

**Step 1: Write all test cases from the spec**

```js
import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { compile } from '../packages/appbase/src/compiler/plugin.js'

// --- Helper ---
function assertIncludes(str, substr, msg) {
  assert.ok(str.includes(substr), msg || `Expected to include: ${substr}`)
}
function assertNotIncludes(str, substr, msg) {
  assert.ok(!str.includes(substr), msg || `Expected NOT to include: ${substr}`)
}

// --- Basic split ---
describe('compiler: basic split', () => {
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
  const [items, setItems] = React.useState([])
  return <div>{items.map(t => <li>{t.text}</li>)}</div>
}

const styles = { wrapper: { color: 'red' } }

serve(App)
`

  it('returns server, client, entryComponent, serverFunctions', () => {
    const result = compile(input)
    assert.ok(result.server)
    assert.ok(result.client)
    assert.equal(result.entryComponent, 'App')
    assert.deepEqual(result.serverFunctions, ['addTodo', 'getTodos'])
  })

  it('server contains only server functions and tainted bindings', () => {
    const result = compile(input)
    assertIncludes(result.server, 'addTodo')
    assertIncludes(result.server, 'getTodos')
    assertIncludes(result.server, "db.collection('todos')")
    assertNotIncludes(result.server, 'App')
    assertNotIncludes(result.server, 'styles')
    assertNotIncludes(result.server, 'serve(')
  })

  it('client contains component, styles, and RPC stubs', () => {
    const result = compile(input)
    assertIncludes(result.client, 'App')
    assertIncludes(result.client, 'styles')
    assertIncludes(result.client, "fetch('/rpc'")
    assertIncludes(result.client, 'jsonrpc')
    assertNotIncludes(result.client, "db.collection")
    assertNotIncludes(result.client, "from 'appbase'")
    assertNotIncludes(result.client, 'serve(App)')
  })
})

// --- "use server" directive ---
describe('compiler: "use server" directive', () => {
  it('per-function directive marks function as server', () => {
    const input = `
import { serve } from 'appbase'
export async function getTime() {
  "use server"
  return Date.now()
}
function App() { return <div>hi</div> }
serve(App)
`
    const result = compile(input)
    assertIncludes(result.server, 'getTime')
    assertIncludes(result.server, 'Date.now()')
    assert.deepEqual(result.serverFunctions, ['getTime'])
    // Client gets RPC stub
    assertIncludes(result.client, "method: 'getTime'")
  })

  it('file-level "use server" makes all exports server functions', () => {
    const input = `
"use server"
import { db } from 'appbase'
const todos = db.collection('todos')
export async function addTodo(text) { return todos.insert({text}) }
export async function getTodos() { return todos.find() }
function helper() { return 'internal' }
`
    const result = compile(input, { target: 'rust' })
    assertIncludes(result.server, 'addTodo')
    assertIncludes(result.server, 'getTodos')
    // helper is not exported, so not an RPC endpoint
    // but it may be included if it references tainted bindings
  })

  it('"use server" directive is stripped from output', () => {
    const input = `
import { serve } from 'appbase'
export async function getTime() {
  "use server"
  return Date.now()
}
function App() { return <div>hi</div> }
serve(App)
`
    const result = compile(input)
    assertNotIncludes(result.server, '"use server"')
    assertNotIncludes(result.client, '"use server"')
  })
})

// --- Taint propagation ---
describe('compiler: taint propagation', () => {
  it('variable initialized from tainted import is tainted', () => {
    const input = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
function App() { return <div>hi</div> }
serve(App)
`
    const result = compile(input)
    assertIncludes(result.server, 'todos')
    assertIncludes(result.server, 'getTodos')
    assertNotIncludes(result.client, "db.collection")
  })

  it('taint does NOT propagate through function calls', () => {
    const input = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
export async function getActive() {
  const all = await getTodos()
  return all.filter(t => !t.done)
}
function App() { return <div>hi</div> }
serve(App)
`
    const result = compile(input)
    // getTodos is server (tainted)
    assertIncludes(result.server, 'getTodos')
    // getActive is CLIENT — calls getTodos but has no direct tainted ref
    assertNotIncludes(result.server, 'getActive')
    // getActive stays in client as-is (getTodos call becomes RPC)
    assertIncludes(result.client, 'getActive')
  })
})

// --- Target: Rust ---
describe('compiler: rust target', () => {
  const input = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
export async function addTodo(text) { return todos.insert({ text }) }
export async function getTodos() { return todos.find() }
function App() { return <div>hi</div> }
serve(App)
`

  it('strips imports, adds globalThis.__rpc', () => {
    const result = compile(input, { target: 'rust' })
    assertNotIncludes(result.server, "import")
    assertNotIncludes(result.server, "from 'appbase'")
    assertNotIncludes(result.server, "export ")
    assertIncludes(result.server, 'globalThis.__rpc')
    assertIncludes(result.server, 'addTodo')
    assertIncludes(result.server, 'getTodos')
  })
})

// --- Target: Node ---
describe('compiler: node target', () => {
  const input = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
export async function addTodo(text) { return todos.insert({ text }) }
export async function getTodos() { return todos.find() }
function App() { return <div>hi</div> }
serve(App)
`

  it('keeps imports, uses export keyword', () => {
    const result = compile(input, { target: 'node' })
    assertIncludes(result.server, "import")
    assertIncludes(result.server, "export async function addTodo")
    assertIncludes(result.server, "export async function getTodos")
    assertNotIncludes(result.server, 'globalThis.__rpc')
  })
})

// --- TypeScript support ---
describe('compiler: typescript', () => {
  it('handles TypeScript syntax', () => {
    const input = `
import { db, serve } from 'appbase'

interface Todo {
  id: string
  text: string
  done: boolean
}

const todos = db.collection('todos')

export async function addTodo(text: string): Promise<Todo> {
  return todos.insert({ text, done: false })
}

export async function getTodos(): Promise<Todo[]> {
  return todos.find()
}

function App(): JSX.Element {
  return <div>hello</div>
}

serve(App)
`
    const result = compile(input)
    assert.ok(result.server)
    assert.ok(result.client)
    assertIncludes(result.server, 'addTodo')
    assertIncludes(result.client, 'App')
  })
})

// --- Edge cases ---
describe('compiler: edge cases', () => {
  it('non-exported server function is server-side helper, not RPC', () => {
    const input = `
import { db, serve } from 'appbase'
const todos = db.collection('todos')
async function internalFind() { return todos.find({ active: true }) }
export async function getActive() {
  "use server"
  return internalFind()
}
function App() { return <div>hi</div> }
serve(App)
`
    const result = compile(input)
    // Both on server
    assertIncludes(result.server, 'internalFind')
    assertIncludes(result.server, 'getActive')
    // Only exported ones are RPC endpoints
    assert.deepEqual(result.serverFunctions, ['getActive'])
    // Client has RPC stub for getActive only
    assertIncludes(result.client, "method: 'getActive'")
    assertNotIncludes(result.client, 'internalFind')
  })

  it('handles no server functions gracefully', () => {
    const input = `
import { serve } from 'appbase'
function App() { return <div>hello</div> }
serve(App)
`
    const result = compile(input)
    assert.equal(result.server, '')
    assert.deepEqual(result.serverFunctions, [])
    assertIncludes(result.client, 'App')
  })

  it('handles no serve() call', () => {
    const input = `
import { db } from 'appbase'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
`
    const result = compile(input)
    assert.equal(result.entryComponent, null)
    assertIncludes(result.server, 'getTodos')
  })
})
```

**Step 2: Run tests to verify they fail**

Run: `node --test tests/compiler.test.js`
Expected: Multiple failures (old compiler doesn't match new spec)

**Step 3: Commit the tests**

```bash
git add tests/compiler.test.js
git commit -m "test: rewrite compiler tests to match v1 spec"
```

---

### Task 2: Rewrite the Compiler — Analysis Passes

**Files:**
- Modify: `packages/appbase/src/compiler/plugin.js`

**Step 1: Implement the new compiler**

The full rewrite with 4 passes:

```js
import { parse } from '@babel/parser'
import traverse from '@babel/traverse'
import generate from '@babel/generator'
import * as t from '@babel/types'

export function compile(source, options = {}) {
  const target = options.target || 'node'

  const ast = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  // --- State ---
  const taintedBindings = new Set()    // e.g. 'db', 'todos'
  const serverFunctions = new Set()    // functions going to server (all detected)
  const exportedServerFns = []         // only exported ones -> RPC endpoints
  let entryComponent = null
  let hasFileDirective = false

  // --- Pass 1: Detect taint sources + file directive + serve() ---
  traverse.default(ast, {
    Program(path) {
      // Check for file-level "use server"
      const firstStmt = path.node.body[0]
      if (
        firstStmt &&
        t.isExpressionStatement(firstStmt) &&
        t.isStringLiteral(firstStmt.expression) &&
        firstStmt.expression.value === 'use server'
      ) {
        hasFileDirective = true
      }
    },
    ImportDeclaration(path) {
      // For now, treat all non-serve imports from 'appbase' as tainted
      // TODO: In future, resolve the source module and check for "use server"
      if (path.node.source.value === 'appbase') {
        for (const spec of path.node.specifiers) {
          const name = spec.local.name
          if (name !== 'serve') {
            taintedBindings.add(name)
          }
        }
      }
    },
    CallExpression(path) {
      if (
        t.isIdentifier(path.node.callee) &&
        path.node.callee.name === 'serve' &&
        path.node.arguments.length > 0
      ) {
        entryComponent = path.node.arguments[0].name || null
      }
    }
  })

  // --- Pass 2: Propagate taint to direct bindings ---
  traverse.default(ast, {
    VariableDeclarator(path) {
      const init = path.node.init
      if (!init || !t.isIdentifier(path.node.id)) return

      // const x = tainted(...)  or  const x = tainted.method(...)
      let sourceIdent = null
      if (t.isCallExpression(init)) {
        if (t.isIdentifier(init.callee)) {
          sourceIdent = init.callee.name
        } else if (t.isMemberExpression(init.callee) && t.isIdentifier(init.callee.object)) {
          sourceIdent = init.callee.object.name
        }
      } else if (t.isMemberExpression(init) && t.isIdentifier(init.object)) {
        sourceIdent = init.object.name
      } else if (t.isIdentifier(init)) {
        sourceIdent = init.name
      }

      if (sourceIdent && taintedBindings.has(sourceIdent)) {
        taintedBindings.add(path.node.id.name)
      }
    }
  })

  // --- Pass 3: Detect server functions ---
  traverse.default(ast, {
    FunctionDeclaration(path) {
      const name = path.node.id?.name
      if (!name) return

      const isExported = t.isExportNamedDeclaration(path.parent)

      // Check for "use server" directive in function body
      const body = path.node.body?.body || []
      const hasDirective = body.some(
        stmt =>
          t.isExpressionStatement(stmt) &&
          t.isStringLiteral(stmt.expression) &&
          stmt.expression.value === 'use server'
      )

      // Check for direct tainted reference
      let hasTaintedRef = false
      path.traverse({
        Identifier(innerPath) {
          if (taintedBindings.has(innerPath.node.name)) {
            // Make sure it's a reference, not a declaration
            if (!innerPath.isBindingIdentifier()) {
              hasTaintedRef = true
            }
          }
        }
      })

      // File-level directive + exported
      const isFileLevelServer = hasFileDirective && isExported

      if (hasDirective || hasTaintedRef || isFileLevelServer) {
        serverFunctions.add(name)
        if (isExported || hasDirective) {
          exportedServerFns.push(name)
        }
      }
    }
  })

  // --- Pass 4a: Generate server code ---
  const serverAst = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  traverse.default(serverAst, {
    // Remove file-level "use server" directive
    Program(path) {
      const firstStmt = path.node.body[0]
      if (
        firstStmt &&
        t.isExpressionStatement(firstStmt) &&
        t.isStringLiteral(firstStmt.expression) &&
        firstStmt.expression.value === 'use server'
      ) {
        path.node.body.shift()
      }
    },

    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        if (target === 'rust') {
          // Rust: strip all imports (runtime injects globals)
          path.remove()
        } else {
          // Node: keep server imports, remove 'serve'
          path.node.specifiers = path.node.specifiers.filter(
            s => s.local.name !== 'serve'
          )
          if (path.node.specifiers.length === 0) {
            path.remove()
          }
        }
      }
    },

    ExportNamedDeclaration(path) {
      const decl = path.node.declaration
      if (t.isFunctionDeclaration(decl)) {
        if (!serverFunctions.has(decl.id.name)) {
          path.remove()
        } else if (target === 'rust') {
          // Rust: remove export keyword
          path.replaceWith(decl)
        }
      }
    },

    FunctionDeclaration(path) {
      // Only remove if it's NOT a server function and not inside ExportNamedDeclaration
      if (!t.isExportNamedDeclaration(path.parent) && !serverFunctions.has(path.node.id?.name)) {
        path.remove()
      }

      // Strip "use server" directive from body
      if (path.node.body?.body) {
        path.node.body.body = path.node.body.body.filter(
          stmt => !(
            t.isExpressionStatement(stmt) &&
            t.isStringLiteral(stmt.expression) &&
            stmt.expression.value === 'use server'
          )
        )
      }
    },

    VariableDeclaration(path) {
      if (t.isExportNamedDeclaration(path.parent)) return
      const decl = path.node.declarations[0]
      if (!decl || !t.isIdentifier(decl.id)) return
      if (!taintedBindings.has(decl.id.name)) {
        path.remove()
      }
    },

    ExpressionStatement(path) {
      // Remove serve() call
      if (
        t.isCallExpression(path.node.expression) &&
        t.isIdentifier(path.node.expression.callee) &&
        path.node.expression.callee.name === 'serve'
      ) {
        path.remove()
      }
    },

    // Remove TypeScript interface/type declarations from server
    TSInterfaceDeclaration(path) { path.remove() },
    TSTypeAliasDeclaration(path) { path.remove() },
  })

  // Rust target: append globalThis.__rpc registration
  let server = generate.default(serverAst).code
  if (target === 'rust' && exportedServerFns.length > 0) {
    server += `\n\nglobalThis.__rpc = { ${exportedServerFns.join(', ')} }`
  }

  // If no server functions, return empty server
  if (serverFunctions.size === 0) {
    server = ''
  }

  // --- Pass 4b: Generate client code ---
  const clientAst = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  traverse.default(clientAst, {
    // Remove file-level "use server" directive
    Program(path) {
      const firstStmt = path.node.body[0]
      if (
        firstStmt &&
        t.isExpressionStatement(firstStmt) &&
        t.isStringLiteral(firstStmt.expression) &&
        firstStmt.expression.value === 'use server'
      ) {
        path.node.body.shift()
      }
    },

    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        path.remove()
      }
    },

    ExpressionStatement(path) {
      if (
        t.isCallExpression(path.node.expression) &&
        t.isIdentifier(path.node.expression.callee) &&
        path.node.expression.callee.name === 'serve'
      ) {
        path.remove()
      }
    },

    VariableDeclaration(path) {
      if (t.isExportNamedDeclaration(path.parent)) return
      const decl = path.node.declarations[0]
      if (decl && t.isIdentifier(decl.id) && taintedBindings.has(decl.id.name)) {
        path.remove()
      }
    },

    ExportNamedDeclaration(path) {
      const decl = path.node.declaration
      if (t.isFunctionDeclaration(decl) && serverFunctions.has(decl.id.name)) {
        // Replace exported server function with RPC stub
        const name = decl.id.name
        const params = decl.params.map(p => {
          if (t.isIdentifier(p)) return p.name
          if (t.isAssignmentPattern(p) && t.isIdentifier(p.left)) return p.left.name
          return '_'
        })
        const rpcStub = buildRpcStub(name, params)
        rpcStub.__rpcReplaced = true
        path.replaceWith(rpcStub)
        path.skip()
      }
    },

    FunctionDeclaration(path) {
      if (path.node.__rpcReplaced) return
      if (serverFunctions.has(path.node.id?.name)) {
        // Non-exported server function: remove from client entirely
        path.remove()
      }

      // Strip "use server" directive from remaining functions
      if (path.node.body?.body) {
        path.node.body.body = path.node.body.body.filter(
          stmt => !(
            t.isExpressionStatement(stmt) &&
            t.isStringLiteral(stmt.expression) &&
            stmt.expression.value === 'use server'
          )
        )
      }
    },
  })

  const client = generate.default(clientAst).code

  return {
    server,
    client,
    entryComponent,
    serverFunctions: exportedServerFns
  }
}

function buildRpcStub(name, params) {
  const paramList = params.join(', ')
  const code = `
    async function ${name}(${paramList}) {
      const res = await fetch('/rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: '${name}',
          params: [${paramList}],
          id: Date.now()
        })
      });
      const data = await res.json();
      if (data.error) throw new Error(data.error.message);
      return data.result;
    }
  `
  return parse(code, { sourceType: 'module' }).program.body[0]
}
```

**Step 2: Run tests**

Run: `node --test tests/compiler.test.js`
Expected: All tests pass

**Step 3: Run ALL tests to check nothing else broke**

Run: `node --test tests/compiler.test.js tests/server.test.js tests/e2e.test.js tests/db.test.js`
Expected: All pass

**Step 4: Commit**

```bash
git add packages/appbase/src/compiler/plugin.js tests/compiler.test.js
git commit -m "feat: rewrite compiler to match v1 spec

- 'use server' directive support (per-function and file-level)
- Taint propagation from 'use server' module imports
- Direct-only taint (no transitive call graph)
- Clean server/client split (no styles leak)
- Target support: 'rust' (no imports, globalThis.__rpc) and 'node' (exports)
- TypeScript support via parser plugin
- Returns serverFunctions list"
```

---

### Task 3: Update Dev Server for New Compiler API

**Files:**
- Modify: `packages/appbase/src/dev.js`

**Step 1: Update `compileAndWrite` to use new `compile` signature**

The `compile` function now takes `(source, { target })` and returns `{ server, client, entryComponent, serverFunctions }`. Update `dev.js` to:

- Pass `{ target: 'node' }` to compile
- Use `serverFunctions` for logging
- No regex hacks for rewriting imports (compiler handles it cleanly now)

**Step 2: Test the dev server**

Run: `node packages/appbase/bin/appbase.js dev examples/todo/app.jsx --port=3001`
Expected: Compiles, starts, API works

**Step 3: Commit**

```bash
git add packages/appbase/src/dev.js
git commit -m "feat: update dev server to use new compiler API"
```

---

### Task 4: Update E2E Test

**Files:**
- Modify: `tests/e2e.test.js`

**Step 1: Update e2e test to verify new compiler output**

Update assertions to match the new spec (e.g. check `serverFunctions` array, verify clean split).

**Step 2: Run all tests**

Run: `node --test tests/compiler.test.js tests/server.test.js tests/e2e.test.js tests/db.test.js`
Expected: All pass

**Step 3: Commit**

```bash
git add tests/e2e.test.js
git commit -m "test: update e2e test for new compiler"
```

---

### Task 5: Verify Rust Target Output

**Files:**
- None new — just verification

**Step 1: Generate Rust target output and compare to spec**

```bash
node -e "
import { compile } from './packages/appbase/src/compiler/plugin.js';
import { readFileSync } from 'fs';
const source = readFileSync('examples/todo/app.jsx', 'utf-8');
const r = compile(source, { target: 'rust' });
console.log('=== SERVER (Rust) ===');
console.log(r.server);
console.log('=== FUNCTIONS ===');
console.log(r.serverFunctions);
"
```

Expected: Server output has no imports, no styles, ends with `globalThis.__rpc = { ... }`

**Step 2: Test with Rust runtime**

```bash
# Save the compiled output and run it on the Rust runtime
node -e "..." > /tmp/test_compiled.js
cargo run -- /tmp/test_compiled.js test.db 3002
curl -X POST http://localhost:3002/rpc -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","method":"getTodos","params":[],"id":1}'
```

**Step 3: Commit if any fixes needed**

---

## Summary

| Task | What | Tests |
|------|------|-------|
| 1 | Write comprehensive tests from spec | 12+ test cases |
| 2 | Rewrite compiler (analysis + code gen) | All compiler tests pass |
| 3 | Update dev server for new API | Dev server works |
| 4 | Update e2e test | All tests pass |
| 5 | Verify Rust target | Manual verification |
