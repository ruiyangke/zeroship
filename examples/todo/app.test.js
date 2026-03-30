import { test } from '../../packages/appbase/src/test.js'
import assert from 'node:assert/strict'

test('addTodo returns a todo with id and text', async ({ rpc }) => {
  const todo = await rpc('addTodo', ['buy milk'])
  assert.equal(todo.text, 'buy milk')
  assert.equal(todo.done, false)
  assert.ok(todo.id)
})

test('getTodos returns all added todos', async ({ rpc }) => {
  await rpc('addTodo', ['one'])
  await rpc('addTodo', ['two'])
  await rpc('addTodo', ['three'])
  const todos = await rpc('getTodos')
  assert.equal(todos.length, 3)
})

test('toggleTodo flips done status', async ({ rpc }) => {
  const todo = await rpc('addTodo', ['toggle me'])
  assert.equal(todo.done, false)
  const updated = await rpc('toggleTodo', [todo.id, true])
  assert.equal(updated.done, true)
})

test('deleteTodo removes the item', async ({ rpc }) => {
  const todo = await rpc('addTodo', ['delete me'])
  const before = await rpc('getTodos')
  await rpc('deleteTodo', [todo.id])
  const after = await rpc('getTodos')
  assert.equal(after.length, before.length - 1)
})

test('getTodos with empty db returns empty array', async ({ rpc }) => {
  const todos = await rpc('getTodos')
  assert.equal(todos.length, 0)
})
