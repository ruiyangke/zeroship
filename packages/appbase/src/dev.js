import { readFileSync, mkdirSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { compile } from './compiler/plugin.js'
import { createDb } from './db.js'
import { createServer as createHonoServer } from './runtime/server.js'
import { createServer as createViteDevServer } from 'vite'
import react from '@vitejs/plugin-react'

export async function dev(entryFile, options = {}) {
  const port = options.port || 3000
  const source = readFileSync(resolve(entryFile), 'utf-8')

  console.log(`[appbase] Compiling ${entryFile}...`)
  const { server: serverCode, client: clientCode, entryComponent } = compile(source)

  // Write compiled output to .dist/
  const distDir = resolve('.dist')
  mkdirSync(resolve(distDir, 'server'), { recursive: true })
  mkdirSync(resolve(distDir, 'client'), { recursive: true })

  writeFileSync(resolve(distDir, 'server/functions.js'), serverCode)
  writeFileSync(resolve(distDir, 'client/App.jsx'), clientCode)

  // Write client index.html in .dist/client/
  writeFileSync(resolve(distDir, 'client/index.html'), `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>appbase app</title>
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body { -webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale; }
    input:focus { border-color: #646cff !important; }
    button:hover { opacity: 0.85; }
  </style>
</head>
<body>
  <div id="root"></div>
  <script type="module" src="/main.jsx"></script>
</body>
</html>`)

  // Write client entry
  writeFileSync(resolve(distDir, 'client/main.jsx'), `
import React from 'react'
import { useState, useEffect } from 'react'
import { createRoot } from 'react-dom/client'

${clientCode.replace(/import\s*\{[^}]*\}\s*from\s*['"]appbase['"];?/g, '')}

const root = createRoot(document.getElementById('root'))
root.render(React.createElement(${entryComponent}))
`)

  console.log(`[appbase] Compiled. Loading server functions...`)

  // Create db and dynamically load server functions
  const db = createDb('appbase.db')
  const serverModulePath = resolve(distDir, 'server/_runtime.js')
  const rewrittenServer = serverCode
    .replace(/import\s*\{[^}]*\}\s*from\s*['"]appbase['"];?/g, '')

  // Extract non-exported function names that need explicit exports
  const nonExportedFns = Array.from(rewrittenServer.matchAll(/(?<!export\s)async\s+function\s+(\w+)/g))
    .map(m => m[1])

  writeFileSync(serverModulePath, `
import { createDb } from '${resolve('packages/appbase/src/db.js')}';
const db = createDb('${resolve('appbase.db')}');
${rewrittenServer}
${nonExportedFns.map(name => `export { ${name} };`).join('\n')}
`)

  const serverModule = await import(serverModulePath + '?t=' + Date.now())
  const functions = {}
  for (const [key, val] of Object.entries(serverModule)) {
    if (typeof val === 'function') functions[key] = val
  }

  console.log(`[appbase] Loaded server functions: ${Object.keys(functions).join(', ')}`)

  // Create Vite dev server in middleware mode
  const vite = await createViteDevServer({
    root: resolve(distDir, 'client'),
    server: { middlewareMode: true },
    plugins: [react()],
    appType: 'spa',
  })

  // Create a combined HTTP server:
  // - /api/* goes to Hono (RPC routes)
  // - Everything else goes to Vite (client bundle + HMR)
  const { createServer: createHttpServer } = await import('node:http')
  const { Hono } = await import('hono')
  const { JSONRPCServer } = await import('json-rpc-2.0')

  const app = new Hono()
  const rpcServer = new JSONRPCServer()

  // Register all functions as JSON-RPC methods
  for (const [name, fn] of Object.entries(functions)) {
    rpcServer.addMethod(name, (params) => fn(...(params || [])))
  }

  // Single JSON-RPC endpoint
  app.post('/rpc', async (c) => {
    const request = await c.req.json()
    const response = await rpcServer.receive(request)
    if (response) return c.json(response)
    return c.body(null, 204)
  })

  // Create Node.js HTTP server
  const httpServer = createHttpServer(async (req, res) => {
    // Try Hono first for API routes
    if (req.url === '/rpc') {
      const response = await app.fetch(new Request(`http://localhost${req.url}`, {
        method: req.method,
        headers: req.headers,
        body: req.method !== 'GET' && req.method !== 'HEAD'
          ? await new Promise((resolve) => {
              let data = ''
              req.on('data', chunk => data += chunk)
              req.on('end', () => resolve(data))
            })
          : undefined,
      }))

      res.writeHead(response.status, Object.fromEntries(response.headers.entries()))
      const body = await response.text()
      res.end(body)
      return
    }

    // Everything else goes to Vite
    vite.middlewares(req, res)
  })

  httpServer.listen(port, () => {
    console.log(`[appbase] Dev server running at http://localhost:${port}`)
    console.log(`[appbase] Vite HMR enabled`)
  })

  return { server: httpServer, vite }
}
