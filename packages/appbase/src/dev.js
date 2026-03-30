import { readFileSync, mkdirSync, writeFileSync, writeFile } from 'node:fs'
import { resolve } from 'node:path'
import { compile } from './compiler/plugin.js'
import { createDb } from './db.js'
import { createServer as createHonoServer } from './runtime/server.js'
import { createServer as createViteDevServer } from 'vite'
import react from '@vitejs/plugin-react'

function compileAndWrite(source, distDir, entryFilePath, dbPath) {
  const { server: serverCode, client: clientCode, entryComponent } = compile(source)

  writeFileSync(resolve(distDir, 'server/functions.js'), serverCode)
  writeFileSync(resolve(distDir, 'client/App.jsx'), clientCode)

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

  writeFileSync(resolve(distDir, 'client/main.jsx'), `
import React from 'react'
import { useState, useEffect } from 'react'
import { createRoot } from 'react-dom/client'

${clientCode.replace(/import\s*\{[^}]*\}\s*from\s*['"]appbase['"];?/g, '')}

const root = createRoot(document.getElementById('root'))
root.render(React.createElement(${entryComponent}))
`)

  const rewrittenServer = serverCode
    .replace(/import\s*\{[^}]*\}\s*from\s*['"]appbase['"];?/g, '')
  const nonExportedFns = Array.from(rewrittenServer.matchAll(/(?<!export\s)async\s+function\s+(\w+)/g))
    .map(m => m[1])

  const serverModulePath = resolve(distDir, 'server/_runtime.js')
  writeFileSync(serverModulePath, `
import { createDb } from '${resolve('packages/appbase/src/db.js')}';
const db = createDb('${dbPath}');
${rewrittenServer}
${nonExportedFns.map(name => `export { ${name} };`).join('\n')}
`)

  return { serverCode, clientCode, entryComponent, serverModulePath }
}

function devShellHtml(port) {
  return `<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>appbase dev</title>
  <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/codemirror.min.css">
  <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/theme/material-darker.min.css">
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    html, body { height: 100%; background: #0f0f0f; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
    #shell { display: flex; height: 100vh; }
    #editor-pane {
      width: 45%;
      min-width: 320px;
      display: flex;
      flex-direction: column;
      border-right: 1px solid #1e1e2e;
      background: #121218;
    }
    #editor-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 10px 16px;
      background: #16161e;
      border-bottom: 1px solid #1e1e2e;
    }
    #editor-header .title {
      font-size: 13px;
      font-weight: 500;
      color: #888;
      letter-spacing: 0.04em;
    }
    #editor-header .logo {
      font-size: 13px;
      color: #646cff;
      font-weight: 600;
      letter-spacing: 0.08em;
    }
    #save-btn {
      padding: 5px 14px;
      font-size: 12px;
      font-weight: 500;
      border: none;
      border-radius: 6px;
      background: #646cff;
      color: #fff;
      cursor: pointer;
      letter-spacing: 0.03em;
      transition: opacity 0.15s;
    }
    #save-btn:hover { opacity: 0.85; }
    #save-btn:disabled { opacity: 0.4; cursor: default; }
    #status {
      font-size: 11px;
      padding: 6px 16px;
      color: #555;
      background: #16161e;
      border-top: 1px solid #1e1e2e;
    }
    #status.error { color: #ff6b6b; }
    #status.success { color: #51cf66; }
    .CodeMirror {
      flex: 1;
      font-size: 13px;
      line-height: 1.6;
      font-family: 'SF Mono', 'Fira Code', 'JetBrains Mono', monospace;
    }
    #preview-pane {
      flex: 1;
      display: flex;
      flex-direction: column;
      background: #0f0f0f;
    }
    #preview-header {
      display: flex;
      align-items: center;
      padding: 10px 16px;
      background: #16161e;
      border-bottom: 1px solid #1e1e2e;
    }
    #preview-header .label {
      font-size: 13px;
      color: #888;
      font-weight: 500;
      letter-spacing: 0.04em;
    }
    #preview-frame {
      flex: 1;
      border: none;
      background: #0f0f0f;
    }
    #resize-handle {
      width: 4px;
      cursor: col-resize;
      background: transparent;
      transition: background 0.15s;
      flex-shrink: 0;
    }
    #resize-handle:hover, #resize-handle.active {
      background: #646cff;
    }
  </style>
</head>
<body>
  <div id="shell">
    <div id="editor-pane">
      <div id="editor-header">
        <span class="logo">&#9671; appbase</span>
        <span class="title" id="filename"></span>
        <button id="save-btn">Save &amp; Run</button>
      </div>
      <textarea id="code"></textarea>
      <div id="status">Ready</div>
    </div>
    <div id="resize-handle"></div>
    <div id="preview-pane">
      <div id="preview-header">
        <span class="label">Preview</span>
      </div>
      <iframe id="preview-frame" src="/__app/"></iframe>
    </div>
  </div>

  <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/codemirror.min.js"></script>
  <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/mode/jsx/jsx.min.js"></script>
  <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/mode/javascript/javascript.min.js"></script>
  <script src="https://cdnjs.cloudflare.com/ajax/libs/codemirror/5.65.18/mode/xml/xml.min.js"></script>
  <script>
    const editor = CodeMirror.fromTextArea(document.getElementById('code'), {
      mode: 'jsx',
      theme: 'material-darker',
      lineNumbers: true,
      tabSize: 2,
      indentWithTabs: false,
      lineWrapping: true,
      autofocus: true,
    });

    const status = document.getElementById('status');
    const saveBtn = document.getElementById('save-btn');
    const filenameEl = document.getElementById('filename');
    const preview = document.getElementById('preview-frame');

    // Load source
    fetch('/__dev/source').then(r => r.json()).then(data => {
      editor.setValue(data.source);
      filenameEl.textContent = data.filename;
    });

    // Save & recompile
    async function save() {
      saveBtn.disabled = true;
      status.textContent = 'Compiling...';
      status.className = '';
      try {
        const res = await fetch('/__dev/source', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ source: editor.getValue() })
        });
        const data = await res.json();
        if (data.error) {
          status.textContent = 'Error: ' + data.error;
          status.className = 'error';
        } else {
          status.textContent = 'Compiled — ' + data.functions.join(', ');
          status.className = 'success';
          preview.src = '/__app/';
        }
      } catch (e) {
        status.textContent = 'Error: ' + e.message;
        status.className = 'error';
      }
      saveBtn.disabled = false;
    }

    saveBtn.addEventListener('click', save);

    // Cmd/Ctrl+S to save
    document.addEventListener('keydown', (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 's') {
        e.preventDefault();
        save();
      }
    });

    // Resize handle
    const handle = document.getElementById('resize-handle');
    const editorPane = document.getElementById('editor-pane');
    let dragging = false;
    handle.addEventListener('mousedown', () => { dragging = true; handle.classList.add('active'); });
    document.addEventListener('mousemove', (e) => {
      if (!dragging) return;
      const width = Math.max(280, Math.min(e.clientX, window.innerWidth - 280));
      editorPane.style.width = width + 'px';
    });
    document.addEventListener('mouseup', () => { dragging = false; handle.classList.remove('active'); });
  </script>
</body>
</html>`
}

export async function dev(entryFile, options = {}) {
  const port = options.port || 3000
  const resolvedEntry = resolve(entryFile)
  let source = readFileSync(resolvedEntry, 'utf-8')

  const distDir = resolve('.dist')
  mkdirSync(resolve(distDir, 'server'), { recursive: true })
  mkdirSync(resolve(distDir, 'client'), { recursive: true })

  const dbPath = resolve('appbase.db')

  console.log(`[appbase] Compiling ${entryFile}...`)
  let compiled = compileAndWrite(source, distDir, resolvedEntry, dbPath)

  console.log(`[appbase] Compiled. Loading server functions...`)

  const serverModule = await import(compiled.serverModulePath + '?t=' + Date.now())
  let functions = {}
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

  const { createServer: createHttpServer } = await import('node:http')
  const { Hono } = await import('hono')
  const { JSONRPCServer } = await import('json-rpc-2.0')

  const app = new Hono()
  let rpcServer = new JSONRPCServer()

  for (const [name, fn] of Object.entries(functions)) {
    rpcServer.addMethod(name, (params) => fn(...(params || [])))
  }

  // JSON-RPC endpoint
  app.post('/rpc', async (c) => {
    const request = await c.req.json()
    const response = await rpcServer.receive(request)
    if (response) return c.json(response)
    return c.body(null, 204)
  })

  // Dev shell: get source
  app.get('/__dev/source', (c) => {
    return c.json({ source, filename: entryFile })
  })

  // Dev shell: save & recompile
  app.post('/__dev/source', async (c) => {
    try {
      const body = await c.req.json()
      source = body.source

      // Save to disk
      writeFileSync(resolvedEntry, source)

      // Recompile
      compiled = compileAndWrite(source, distDir, resolvedEntry, dbPath)

      // Reload server functions
      const newModule = await import(compiled.serverModulePath + '?t=' + Date.now())
      functions = {}
      for (const [key, val] of Object.entries(newModule)) {
        if (typeof val === 'function') functions[key] = val
      }

      // Rebuild RPC server
      rpcServer = new JSONRPCServer()
      for (const [name, fn] of Object.entries(functions)) {
        rpcServer.addMethod(name, (params) => fn(...(params || [])))
      }

      console.log(`[appbase] Recompiled. Functions: ${Object.keys(functions).join(', ')}`)
      return c.json({ ok: true, functions: Object.keys(functions) })
    } catch (e) {
      console.error(`[appbase] Compile error:`, e.message)
      return c.json({ error: e.message }, 400)
    }
  })

  // Create Node.js HTTP server
  const httpServer = createHttpServer(async (req, res) => {
    // JSON-RPC
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

    // Dev shell endpoints
    if (req.url.startsWith('/__dev/')) {
      const reqBody = req.method === 'POST'
        ? await new Promise((resolve) => {
            let data = ''
            req.on('data', chunk => data += chunk)
            req.on('end', () => resolve(data))
          })
        : undefined
      const response = await app.fetch(new Request(`http://localhost${req.url}`, {
        method: req.method,
        headers: req.headers,
        body: reqBody,
      }))
      res.writeHead(response.status, Object.fromEntries(response.headers.entries()))
      res.end(await response.text())
      return
    }

    // Dev shell UI
    if (req.url === '/' || req.url === '/__dev') {
      res.writeHead(200, { 'Content-Type': 'text/html' })
      res.end(devShellHtml(port))
      return
    }

    // App preview (served under /__app/)
    if (req.url.startsWith('/__app')) {
      req.url = req.url.replace(/^\/__app/, '') || '/'
    }

    // Everything else goes to Vite
    vite.middlewares(req, res)
  })

  httpServer.listen(port, () => {
    console.log(`[appbase] Dev server running at http://localhost:${port}`)
    console.log(`[appbase] Editor + Preview at http://localhost:${port}`)
  })

  return { server: httpServer, vite }
}
