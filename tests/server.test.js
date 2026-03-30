import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from '../packages/appbase/src/runtime/server.js'
import { createDb } from '../packages/appbase/src/db.js'

function rpc(address, method, params = [], id = 1) {
  return fetch(`${address}/rpc`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', method, params, id })
  }).then(r => r.json())
}

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

  it('calls a method via JSON-RPC', async () => {
    const res = await rpc(address, 'addTodo', ['buy milk'])
    assert.equal(res.jsonrpc, '2.0')
    assert.equal(res.id, 1)
    assert.ok(res.result.id)
    assert.equal(res.result.text, 'buy milk')
  })

  it('returns results from getTodos', async () => {
    const res = await rpc(address, 'getTodos', [], 2)
    assert.equal(res.jsonrpc, '2.0')
    assert.ok(Array.isArray(res.result))
    assert.ok(res.result.length > 0)
  })

  it('returns error for unknown method', async () => {
    const res = await rpc(address, 'noSuchFn', [], 3)
    assert.equal(res.jsonrpc, '2.0')
    assert.ok(res.error)
    assert.equal(res.error.code, -32601) // Method not found
  })

  it('supports batch requests', async () => {
    const batch = [
      { jsonrpc: '2.0', method: 'addTodo', params: ['item 1'], id: 10 },
      { jsonrpc: '2.0', method: 'addTodo', params: ['item 2'], id: 11 },
      { jsonrpc: '2.0', method: 'getTodos', params: [], id: 12 },
    ]
    const res = await fetch(`${address}/rpc`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(batch)
    }).then(r => r.json())

    assert.ok(Array.isArray(res))
    assert.equal(res.length, 3)
    assert.equal(res[0].result.text, 'item 1')
    assert.equal(res[1].result.text, 'item 2')
    assert.ok(res[2].result.length >= 3) // at least the 3 we added
  })
})
