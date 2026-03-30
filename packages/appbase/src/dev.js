import { readFileSync, mkdirSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { compile } from './compiler/plugin.js'
import { createDb } from './db.js'
import { createServer } from './runtime/server.js'

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

  // Write client index.html
  writeFileSync(resolve(distDir, 'client/index.html'), `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>appbase app</title></head>
<body>
  <div id="root"></div>
  <script type="module" src="/main.jsx"></script>
</body>
</html>`)

  // Write client entry that mounts the component
  writeFileSync(resolve(distDir, 'client/main.jsx'), `
import React from 'react'
import { createRoot } from 'react-dom/client'
${clientCode}

const root = createRoot(document.getElementById('root'))
root.render(React.createElement(${entryComponent}))
`)

  console.log(`[appbase] Server code:\n${serverCode}\n`)
  console.log(`[appbase] Client code:\n${clientCode}\n`)

  // Create db and dynamically load server functions
  const db = createDb('appbase.db')

  // Build server functions by writing a proper ES module and importing it
  // Rewrite the server code to replace `import { db } from 'appbase'` with actual db import
  const serverModulePath = resolve(distDir, 'server/_runtime.js')

  const rewrittenServer = serverCode
    .replace(/import\s*\{[^}]*\}\s*from\s*['"]appbase['"];?/g, '')

  // Extract function names from the rewritten server code
  const functionNames = Array.from(rewrittenServer.matchAll(/(?:export\s+)?async\s+function\s+(\w+)/g))
    .map(m => m[1])

  writeFileSync(serverModulePath, `
import { createDb } from '${resolve('packages/appbase/src/db.js')}';
const db = createDb('${resolve('appbase.db')}');
${rewrittenServer}
${functionNames.map(name => `export { ${name} };`).join('\n')}
`)

  const serverModule = await import(serverModulePath)

  // Collect all exported functions
  const functions = {}
  for (const [key, val] of Object.entries(serverModule)) {
    if (typeof val === 'function') {
      functions[key] = val
    }
  }

  console.log(`[appbase] Loaded server functions: ${Object.keys(functions).join(', ')}`)

  // Start server
  const { address } = createServer({
    functions,
    port,
    staticDir: resolve(distDir, 'client')
  })

  console.log(`[appbase] Dev server running at ${address}`)
  return { address }
}
