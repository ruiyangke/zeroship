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

  it('client code removes serve() call and appbase imports', () => {
    const result = compile(input)
    assert.ok(!result.client.includes('serve(App)'))
    assert.ok(!result.client.includes("from 'appbase'"))
  })
})
