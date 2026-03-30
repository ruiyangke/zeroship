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
