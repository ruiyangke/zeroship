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
    const compiled = {
      path: route.path,
      layouts: route.layouts,
      layoutChain: route.layouts,
      params: route.params,
      page: null,
      layout: null,
      loading: null,
      error: null,
      notFound: null,
      template: null,
    }

    // Compile server.js if present (implicitly "use server")
    if (route.files.server) {
      const source = readFileSync(route.files.server, 'utf-8')
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

    // Read layout (client-only, no compilation needed for now)
    if (route.files.layout) {
      compiled.layout = readFileSync(route.files.layout, 'utf-8')
    }

    // Read other special files
    for (const key of ['loading', 'error', 'notFound', 'template']) {
      if (route.files[key]) {
        compiled[key] = readFileSync(route.files[key], 'utf-8')
      }
    }

    compiledRoutes.push(compiled)
  }

  const uniqueServerFunctions = [...new Set(allServerFunctions)]
  const serverBundle = serverChunks.join('\n\n')

  return {
    routes: compiledRoutes,
    serverBundle,
    serverFunctions: uniqueServerFunctions,
  }
}
