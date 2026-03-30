import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { compile } from '../packages/appbase/src/compiler/plugin.js'

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
    assertIncludes(result.server, 'getTodos')
    assertNotIncludes(result.server, 'getActive')
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
    assertIncludes(result.server, 'internalFind')
    assertIncludes(result.server, 'getActive')
    assert.deepEqual(result.serverFunctions, ['getActive'])
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
