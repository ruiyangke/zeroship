import { Hono } from 'hono'
import { serve } from '@hono/node-server'
import { serveStatic } from '@hono/node-server/serve-static'

export function createServer({ functions, port = 3000, staticDir = null }) {
  const app = new Hono()

  // Register RPC routes for each server function
  app.post('/api/:name', async (c) => {
    const name = c.req.param('name')
    const fn = functions[name]
    if (!fn) {
      return c.json({ error: `Function '${name}' not found` }, 404)
    }

    const body = await c.req.json()
    const args = body.args || []
    const result = await fn(...args)
    return c.json(result)
  })

  // Serve static client files if dir is provided
  if (staticDir) {
    app.use('/*', serveStatic({ root: staticDir }))
  }

  const httpServer = serve({ fetch: app.fetch, port }, (info) => {
    // Server started
  })

  const address = `http://localhost:${httpServer.address().port}`

  return { server: httpServer, address, app }
}
