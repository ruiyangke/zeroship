import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { compile } from '../packages/appbase/src/compiler/plugin.js'
import { createDb } from '../packages/appbase/src/db.js'
import { createServer } from '../packages/appbase/src/runtime/server.js'

function rpc(address, method, params = [], id = 1) {
  return fetch(`${address}/rpc`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', method, params, id })
  }).then(r => r.json())
}

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
    assert.ok(clientCode.includes("fetch('/rpc'"))
    assert.ok(clientCode.includes('jsonrpc'))
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

  it('compiles and the JSON-RPC API works end to end', async () => {
    const add = await rpc(address, 'addTodo', ['hello world'], 1)
    assert.equal(add.result.text, 'hello world')

    const list = await rpc(address, 'getTodos', [], 2)
    assert.equal(list.result.length, 1)
    assert.equal(list.result[0].text, 'hello world')
  })
})
