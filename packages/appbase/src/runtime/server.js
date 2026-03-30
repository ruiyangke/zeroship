import { Hono } from 'hono'
import { serve } from '@hono/node-server'
import { serveStatic } from '@hono/node-server/serve-static'
import { JSONRPCServer } from 'json-rpc-2.0'

export function createServer({ functions, port = 3000, staticDir = null }) {
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
    if (response) {
      return c.json(response)
    }
    return c.body(null, 204)
  })

  // Serve static client files if dir is provided
  if (staticDir) {
    app.use('/*', serveStatic({ root: staticDir }))
  }

  const httpServer = serve({ fetch: app.fetch, port }, (info) => {
    // Server started
  })

  const address = `http://localhost:${httpServer.address().port}`

  return { server: httpServer, address, app, rpcServer }
}
