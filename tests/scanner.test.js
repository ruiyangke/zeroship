import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { scanRoutes } from '../packages/appbase/src/compiler/scanner.js'
import { mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'

function setupFixture(base, files) {
  for (const [path, content] of Object.entries(files)) {
    const full = join(base, path)
    mkdirSync(join(full, '..'), { recursive: true })
    writeFileSync(full, content)
  }
}

describe('scanner', () => {
  const fixture = '/tmp/appbase-test-scanner-' + Date.now()

  before(() => {
    mkdirSync(fixture, { recursive: true })
    setupFixture(fixture, {
      'page.jsx': 'export default function Home() {}',
      'layout.jsx': 'export default function Layout({ children }) {}',
      'loading.jsx': 'export default function Loading() {}',
      'error.jsx': 'export default function Error() {}',
      'not-found.jsx': 'export default function NotFound() {}',
      'about/page.jsx': 'export default function About() {}',
      'todos/page.jsx': 'export default function Todos() {}',
      'todos/layout.jsx': 'export default function TodosLayout({ children }) {}',
      'todos/server.js': 'export async function getTodos() {}',
      'todos/loading.jsx': 'export default function TodosLoading() {}',
      'todos/[id]/page.jsx': 'export default function TodoDetail() {}',
      'todos/[id]/server.js': 'export async function getTodo() {}',
      'settings/page.jsx': 'export default function Settings() {}',
      'settings/profile/page.jsx': 'export default function Profile() {}',
      'components/Header.jsx': 'export default function Header() {}',
      'lib/utils.js': 'export function format() {}',
    })
  })

  after(() => {
    rmSync(fixture, { recursive: true, force: true })
  })

  it('discovers all routes with page.jsx', () => {
    const routes = scanRoutes(fixture)
    const paths = routes.map(r => r.path).sort()
    assert.deepEqual(paths, [
      '/',
      '/about',
      '/settings',
      '/settings/profile',
      '/todos',
      '/todos/:id',
    ])
  })

  it('includes special files for each route', () => {
    const routes = scanRoutes(fixture)
    const root = routes.find(r => r.path === '/')
    assert.ok(root.files.page)
    assert.ok(root.files.layout)
    assert.ok(root.files.loading)
    assert.ok(root.files.error)
    assert.ok(root.files.notFound)
  })

  it('includes colocated server.js', () => {
    const routes = scanRoutes(fixture)
    const todos = routes.find(r => r.path === '/todos')
    assert.ok(todos.files.server)
    const todoDetail = routes.find(r => r.path === '/todos/:id')
    assert.ok(todoDetail.files.server)
  })

  it('ignores non-route directories (components, lib)', () => {
    const routes = scanRoutes(fixture)
    const paths = routes.map(r => r.path)
    assert.ok(!paths.includes('/components'))
    assert.ok(!paths.includes('/lib'))
  })

  it('converts [param] to :param in path', () => {
    const routes = scanRoutes(fixture)
    const dynamic = routes.find(r => r.path === '/todos/:id')
    assert.ok(dynamic)
    assert.deepEqual(dynamic.params, ['id'])
  })

  it('builds layout hierarchy', () => {
    const routes = scanRoutes(fixture)
    const todoDetail = routes.find(r => r.path === '/todos/:id')
    assert.equal(todoDetail.layouts.length, 2)
  })
})
