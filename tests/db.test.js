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
