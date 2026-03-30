# Multi-File App Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add file-system routing support so appbase apps can use a Next.js App Router style directory structure with pages, layouts, loading states, error boundaries, and colocated server modules.

**Architecture:** Three new modules: (1) a route scanner that walks the `app/` directory and discovers routes, (2) a directory compiler that compiles each file using the existing single-file compiler, (3) a client-side router that renders the correct page/layout hierarchy based on URL. The existing single-file mode is preserved.

**Tech Stack:** Babel (existing compiler), React (client router with Suspense + ErrorBoundary), Node.js fs for directory scanning

---

### Task 1: Route Scanner — Discover Routes from Directory

**Files:**
- Create: `packages/appbase/src/compiler/scanner.js`
- Create: `tests/scanner.test.js`

**Step 1: Write tests**

```js
import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { scanRoutes } from '../packages/appbase/src/compiler/scanner.js'
import { mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'

function setupFixture(base, files) {
  mkdirSync(base, { recursive: true })
  for (const [path, content] of Object.entries(files)) {
    const full = join(base, path)
    mkdirSync(join(full, '..'), { recursive: true })
    writeFileSync(full, content)
  }
}

describe('scanner', () => {
  const fixture = '/tmp/appbase-test-scanner'

  before(() => {
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
    assert.equal(dynamic.params[0], 'id')
  })

  it('builds layout hierarchy', () => {
    const routes = scanRoutes(fixture)
    const todoDetail = routes.find(r => r.path === '/todos/:id')
    // Should inherit layouts: root layout -> todos layout
    assert.equal(todoDetail.layouts.length, 2)
  })
})
```

**Step 2: Implement scanner**

```js
// packages/appbase/src/compiler/scanner.js
import { readdirSync, statSync, existsSync, readFileSync } from 'node:fs'
import { join, relative } from 'node:path'

const SPECIAL_FILES = {
  'page.jsx': 'page',
  'page.tsx': 'page',
  'layout.jsx': 'layout',
  'layout.tsx': 'layout',
  'loading.jsx': 'loading',
  'loading.tsx': 'loading',
  'error.jsx': 'error',
  'error.tsx': 'error',
  'not-found.jsx': 'notFound',
  'not-found.tsx': 'notFound',
  'template.jsx': 'template',
  'template.tsx': 'template',
  'server.js': 'server',
  'server.ts': 'server',
}

export function scanRoutes(appDir) {
  const routes = []
  walkDir(appDir, appDir, [], routes)
  return routes
}

function walkDir(dir, appDir, parentLayouts, routes) {
  const entries = readdirSync(dir).sort()
  const files = {}
  const layouts = [...parentLayouts]

  // First pass: collect special files in this directory
  for (const entry of entries) {
    const fullPath = join(dir, entry)
    if (statSync(fullPath).isFile() && SPECIAL_FILES[entry]) {
      files[SPECIAL_FILES[entry]] = fullPath
    }
  }

  // If this dir has a layout, add it to the chain
  if (files.layout) {
    layouts.push(files.layout)
  }

  // If this dir has a page, it's a route
  if (files.page) {
    const relPath = relative(appDir, dir)
    const urlPath = dirToUrlPath(relPath)
    const params = extractParams(relPath)

    routes.push({
      path: urlPath,
      dir: dir,
      files,
      layouts,
      params,
    })
  }

  // Recurse into subdirectories
  for (const entry of entries) {
    const fullPath = join(dir, entry)
    if (statSync(fullPath).isDirectory()) {
      // Skip non-route dirs that are clearly not routes
      // (but still recurse — a nested dir might have page.jsx)
      walkDir(fullPath, appDir, layouts, routes)
    }
  }
}

function dirToUrlPath(relPath) {
  if (relPath === '' || relPath === '.') return '/'

  const segments = relPath.split('/').map(seg => {
    // Route groups: (name) -> ignored in URL
    if (seg.startsWith('(') && seg.endsWith(')')) return null
    // Catch-all: [...param] -> *
    if (seg.startsWith('[...') && seg.endsWith(']')) return '*'
    // Dynamic: [param] -> :param
    if (seg.startsWith('[') && seg.endsWith(']')) return ':' + seg.slice(1, -1)
    return seg
  }).filter(Boolean)

  return '/' + segments.join('/')
}

function extractParams(relPath) {
  if (!relPath) return []
  return relPath.split('/').filter(seg =>
    seg.startsWith('[') && seg.endsWith(']')
  ).map(seg => {
    if (seg.startsWith('[...')) return seg.slice(4, -1)
    return seg.slice(1, -1)
  })
}
```

**Step 3: Run tests**

Run: `node --test tests/scanner.test.js`
Expected: All 6 tests pass

**Step 4: Commit**

```bash
git add packages/appbase/src/compiler/scanner.js tests/scanner.test.js
git commit -m "feat: add route scanner for file-system routing"
```

---

### Task 2: Directory Compiler — Compile All Routes

**Files:**
- Create: `packages/appbase/src/compiler/directory.js`
- Create: `tests/directory-compiler.test.js`

**Step 1: Write tests**

```js
import { describe, it, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { compileDirectory } from '../packages/appbase/src/compiler/directory.js'
import { mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'

function setupFixture(base, files) {
  mkdirSync(base, { recursive: true })
  for (const [path, content] of Object.entries(files)) {
    const full = join(base, path)
    mkdirSync(join(full, '..'), { recursive: true })
    writeFileSync(full, content)
  }
}

describe('directory compiler', () => {
  const fixture = '/tmp/appbase-test-dircompiler'

  before(() => {
    setupFixture(fixture, {
      'layout.jsx': `
export default function Layout({ children }) {
  return <html><body>{children}</body></html>
}`,
      'page.jsx': `
export default function Home() {
  return <h1>Home</h1>
}`,
      'todos/page.jsx': `
import { getTodos } from './server'
export default function Todos() {
  return <div>Todos</div>
}`,
      'todos/server.js': `
import { db } from 'appbase'
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
  })

  it('produces route manifest', () => {
    const result = compileDirectory(fixture)
    const root = result.routes.find(r => r.path === '/')
    assert.ok(root)
    assert.ok(root.page)
    const todos = result.routes.find(r => r.path === '/todos')
    assert.ok(todos)
  })

  it('server.js exports become RPC stubs in page client code', () => {
    const result = compileDirectory(fixture)
    const todos = result.routes.find(r => r.path === '/todos')
    assert.ok(todos.page.includes("fetch('/rpc'") || todos.page.includes('getTodos'))
  })
})
```

**Step 2: Implement directory compiler**

```js
// packages/appbase/src/compiler/directory.js
import { readFileSync } from 'node:fs'
import { scanRoutes } from './scanner.js'
import { compile } from './plugin.js'

export function compileDirectory(appDir, options = {}) {
  const target = options.target || 'node'
  const scannedRoutes = scanRoutes(appDir)

  const allServerFunctions = []
  const serverChunks = []
  const compiledRoutes = []

  for (const route of scannedRoutes) {
    const compiled = { path: route.path, layouts: [], params: route.params }

    // Compile server.js if present (implicitly "use server")
    if (route.files.server) {
      const source = readFileSync(route.files.server, 'utf-8')
      // Prepend "use server" if not already there
      const serverSource = source.trimStart().startsWith('"use server"')
        ? source
        : `"use server"\n${source}`
      const result = compile(serverSource, { target })
      if (result.server) {
        serverChunks.push(result.server)
      }
      allServerFunctions.push(...result.serverFunctions)
    }

    // Compile page.jsx
    if (route.files.page) {
      const source = readFileSync(route.files.page, 'utf-8')
      const result = compile(source, { target })
      compiled.page = result.client
      if (result.server) {
        serverChunks.push(result.server)
      }
      allServerFunctions.push(...result.serverFunctions)
    }

    // Compile layout.jsx
    if (route.files.layout) {
      const source = readFileSync(route.files.layout, 'utf-8')
      compiled.layout = source // Layouts are client-only for now
    }

    // Compile other special files (loading, error, not-found, template)
    for (const key of ['loading', 'error', 'notFound', 'template']) {
      if (route.files[key]) {
        compiled[key] = readFileSync(route.files[key], 'utf-8')
      }
    }

    // Layout chain (file paths)
    compiled.layoutChain = route.layouts

    compiledRoutes.push(compiled)
  }

  // Deduplicate server functions
  const uniqueServerFunctions = [...new Set(allServerFunctions)]

  // Combine server chunks
  const serverBundle = serverChunks.join('\n\n')

  return {
    routes: compiledRoutes,
    serverBundle,
    serverFunctions: uniqueServerFunctions,
  }
}
```

**Step 3: Run tests**

Run: `node --test tests/directory-compiler.test.js`
Expected: All 5 tests pass

**Step 4: Commit**

```bash
git add packages/appbase/src/compiler/directory.js tests/directory-compiler.test.js
git commit -m "feat: add directory compiler for multi-file apps"
```

---

### Task 3: Client-Side Router

**Files:**
- Create: `packages/appbase/src/client/router.jsx`
- Create: `tests/router.test.js`

**Step 1: Write the router**

A minimal client-side router using React with Suspense and ErrorBoundary support:

```jsx
// packages/appbase/src/client/router.jsx
import React, { useState, useEffect, Suspense, Component } from 'react'

// Error Boundary component
class ErrorBoundary extends Component {
  constructor(props) {
    super(props)
    this.state = { error: null }
  }
  static getDerivedStateFromError(error) {
    return { error }
  }
  render() {
    if (this.state.error) {
      const ErrorComponent = this.props.fallback
      if (ErrorComponent) {
        return <ErrorComponent
          error={this.state.error}
          reset={() => this.setState({ error: null })}
        />
      }
      return <div>Error: {this.state.error.message}</div>
    }
    return this.props.children
  }
}

// Match a URL path against a route pattern
export function matchRoute(pattern, pathname) {
  const patternParts = pattern.split('/').filter(Boolean)
  const pathParts = pathname.split('/').filter(Boolean)

  // Catch-all
  if (patternParts.includes('*')) {
    const starIdx = patternParts.indexOf('*')
    const prefix = patternParts.slice(0, starIdx)
    for (let i = 0; i < prefix.length; i++) {
      if (prefix[i].startsWith(':')) continue
      if (prefix[i] !== pathParts[i]) return null
    }
    return { slug: pathParts.slice(starIdx) }
  }

  if (patternParts.length !== pathParts.length) return null

  const params = {}
  for (let i = 0; i < patternParts.length; i++) {
    if (patternParts[i].startsWith(':')) {
      params[patternParts[i].slice(1)] = pathParts[i]
    } else if (patternParts[i] !== pathParts[i]) {
      return null
    }
  }
  return params
}

// Router component
export function Router({ routes }) {
  const [pathname, setPathname] = useState(window.location.pathname)

  useEffect(() => {
    const onPopState = () => setPathname(window.location.pathname)
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [])

  // Navigate function (exposed globally)
  useEffect(() => {
    window.__navigate = (to) => {
      window.history.pushState({}, '', to)
      setPathname(to)
    }
  }, [])

  // Find matching route
  let matched = null
  let params = {}
  for (const route of routes) {
    const result = matchRoute(route.path, pathname)
    if (result !== null) {
      matched = route
      params = result
      break
    }
  }

  if (!matched) {
    // Try to find a not-found component from root route
    const root = routes.find(r => r.path === '/')
    if (root && root.notFound) {
      return root.notFound
    }
    return React.createElement('div', null, '404 Not Found')
  }

  // Build the component tree: layouts -> template -> loading/error -> page
  let element = React.createElement(matched.component, { params })

  // Wrap with error boundary
  if (matched.error) {
    element = React.createElement(ErrorBoundary, { fallback: matched.error }, element)
  }

  // Wrap with suspense (loading)
  if (matched.loading) {
    element = React.createElement(Suspense, {
      fallback: React.createElement(matched.loading)
    }, element)
  }

  // Wrap with template (re-mounts on navigation)
  if (matched.template) {
    element = React.createElement(matched.template, { key: pathname }, element)
  }

  // Wrap with layout chain (innermost to outermost)
  const layouts = matched.layouts || []
  for (let i = layouts.length - 1; i >= 0; i--) {
    const Layout = layouts[i]
    element = React.createElement(Layout, { children: element })
  }

  return element
}

// Link component for client-side navigation
export function Link({ to, children, ...props }) {
  const handleClick = (e) => {
    e.preventDefault()
    window.__navigate(to)
  }
  return React.createElement('a', { href: to, onClick: handleClick, ...props }, children)
}
```

**Step 2: Write router tests**

```js
// tests/router.test.js
import { describe, it } from 'node:test'
import assert from 'node:assert/strict'

// We can test matchRoute without a browser
// Import using dynamic import to handle JSX
const { matchRoute } = await import('../packages/appbase/src/client/router.jsx')

describe('router: matchRoute', () => {
  it('matches exact paths', () => {
    assert.deepEqual(matchRoute('/', '/'), {})
    assert.deepEqual(matchRoute('/about', '/about'), {})
    assert.equal(matchRoute('/about', '/other'), null)
  })

  it('matches dynamic segments', () => {
    assert.deepEqual(matchRoute('/todos/:id', '/todos/abc'), { id: 'abc' })
    assert.deepEqual(matchRoute('/users/:userId/posts/:postId', '/users/1/posts/2'), { userId: '1', postId: '2' })
    assert.equal(matchRoute('/todos/:id', '/todos'), null)
  })

  it('matches catch-all', () => {
    const result = matchRoute('/blog/*', '/blog/2024/hello-world')
    assert.ok(result)
    assert.deepEqual(result.slug, ['2024', 'hello-world'])
  })

  it('rejects mismatched paths', () => {
    assert.equal(matchRoute('/about', '/about/team'), null)
    assert.equal(matchRoute('/about/team', '/about'), null)
  })
})
```

**Step 3: Run tests**

Run: `node --test tests/router.test.js`
Expected: All 4 tests pass

**Step 4: Commit**

```bash
git add packages/appbase/src/client/router.jsx tests/router.test.js
git commit -m "feat: add client-side router with layout nesting and error boundaries"
```

---

### Task 4: Example Multi-File App

**Files:**
- Create: `examples/multi/app/layout.jsx`
- Create: `examples/multi/app/page.jsx`
- Create: `examples/multi/app/about/page.jsx`
- Create: `examples/multi/app/todos/page.jsx`
- Create: `examples/multi/app/todos/server.js`
- Create: `examples/multi/app/todos/[id]/page.jsx`

**Step 1: Create the example app**

A multi-page todo app demonstrating the convention.

Root layout:
```jsx
// examples/multi/app/layout.jsx
export default function RootLayout({ children }) {
  return (
    <html>
      <body style={{ fontFamily: 'sans-serif', margin: 0, background: '#0f0f0f', color: '#e0e0e0' }}>
        <nav style={{ padding: '16px 24px', borderBottom: '1px solid #222', display: 'flex', gap: '16px' }}>
          <a href="/" style={{ color: '#646cff', textDecoration: 'none' }}>Home</a>
          <a href="/about" style={{ color: '#646cff', textDecoration: 'none' }}>About</a>
          <a href="/todos" style={{ color: '#646cff', textDecoration: 'none' }}>Todos</a>
        </nav>
        <main style={{ padding: '24px' }}>
          {children}
        </main>
      </body>
    </html>
  )
}
```

Home page, about page, todos page with server functions, and dynamic todo detail page.

**Step 2: Commit**

```bash
git add examples/multi/
git commit -m "feat: add example multi-file app"
```

---

### Task 5: Wire Up Dev Server for Directory Mode

**Files:**
- Modify: `packages/appbase/src/dev.js`
- Modify: `packages/appbase/bin/appbase.js`

**Step 1: Update CLI to detect directory vs single file**

```js
// If the entry is a directory, use directory mode
// If it's a file, use single-file mode (existing behavior)
const entry = args[0] || 'app.jsx'
const stat = statSync(entry, { throwIfNoEntry: false })
if (stat?.isDirectory()) {
  devDirectory(entry, { port })
} else {
  dev(entry, { port })
}
```

**Step 2: Add `devDirectory` function**

Uses `compileDirectory` to compile all routes, generates a client bundle that includes the router + all page components, and serves everything through the existing Hono + Vite setup.

**Step 3: Test manually**

Run: `node packages/appbase/bin/appbase.js dev examples/multi/app --port=3001`
Expected: Multi-page app with client-side routing, todos with server functions

**Step 4: Commit**

```bash
git add packages/appbase/src/dev.js packages/appbase/bin/appbase.js
git commit -m "feat: add directory mode to dev server"
```

---

## Summary

| Task | What | Tests |
|------|------|-------|
| 1 | Route scanner — discover routes from directory | 6 tests |
| 2 | Directory compiler — compile all routes | 5 tests |
| 3 | Client-side router with layouts + error boundaries | 4 tests |
| 4 | Example multi-file app | Manual |
| 5 | Wire up dev server for directory mode | Manual |
