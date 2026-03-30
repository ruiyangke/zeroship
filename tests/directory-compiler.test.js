import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { compileDirectory } from '../packages/appbase/src/compiler/directory.js'
import { mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'

function setupFixture(base, files) {
  for (const [path, content] of Object.entries(files)) {
    const full = join(base, path)
    mkdirSync(join(full, '..'), { recursive: true })
    writeFileSync(full, content)
  }
}

describe('directory compiler', () => {
  const fixture = '/tmp/appbase-test-dircompiler-' + Date.now()

  before(() => {
    mkdirSync(fixture, { recursive: true })
    setupFixture(fixture, {
      'layout.jsx': `export default function Layout({ children }) {
  return <html><body>{children}</body></html>
}`,
      'page.jsx': `export default function Home() {
  return <h1>Home</h1>
}`,
      'todos/page.jsx': `export default function Todos() {
  return <div>Todos</div>
}`,
      'todos/server.js': `import { db } from 'appbase'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
export async function addTodo(text) { return todos.insert({ text, done: false }) }
`,
    })
  })

  after(() => {
    rmSync(fixture, { recursive: true, force: true })
  })

  it('compiles all routes', () => {
    const result = compileDirectory(fixture)
    assert.ok(result.routes.length >= 2)
    const paths = result.routes.map(r => r.path).sort()
    assert.deepEqual(paths, ['/', '/todos'])
  })

  it('collects all server functions', () => {
    const result = compileDirectory(fixture)
    assert.ok(result.serverFunctions.includes('getTodos'))
    assert.ok(result.serverFunctions.includes('addTodo'))
  })

  it('produces server bundle with all server functions', () => {
    const result = compileDirectory(fixture)
    assert.ok(result.serverBundle.includes('getTodos'))
    assert.ok(result.serverBundle.includes('addTodo'))
    assert.ok(result.serverBundle.includes("db.collection"))
  })

  it('produces page client code for each route', () => {
    const result = compileDirectory(fixture)
    const root = result.routes.find(r => r.path === '/')
    assert.ok(root.page)
    assert.ok(root.page.includes('Home'))
    const todos = result.routes.find(r => r.path === '/todos')
    assert.ok(todos.page)
    assert.ok(todos.page.includes('Todos'))
  })

  it('includes layout source for routes that have one', () => {
    const result = compileDirectory(fixture)
    const root = result.routes.find(r => r.path === '/')
    assert.ok(root.layout)
    assert.ok(root.layout.includes('Layout'))
  })
})
