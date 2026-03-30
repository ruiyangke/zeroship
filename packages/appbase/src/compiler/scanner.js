import { readdirSync, statSync } from 'node:fs'
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

  for (const entry of entries) {
    const fullPath = join(dir, entry)
    if (statSync(fullPath).isFile() && SPECIAL_FILES[entry]) {
      files[SPECIAL_FILES[entry]] = fullPath
    }
  }

  if (files.layout) {
    layouts.push(files.layout)
  }

  if (files.page) {
    const relPath = relative(appDir, dir)
    const urlPath = dirToUrlPath(relPath)
    const params = extractParams(relPath)
    routes.push({ path: urlPath, dir, files, layouts, params })
  }

  for (const entry of entries) {
    const fullPath = join(dir, entry)
    if (statSync(fullPath).isDirectory()) {
      walkDir(fullPath, appDir, layouts, routes)
    }
  }
}

function dirToUrlPath(relPath) {
  if (relPath === '' || relPath === '.') return '/'
  const segments = relPath.split('/').map(seg => {
    if (seg.startsWith('(') && seg.endsWith(')')) return null
    if (seg.startsWith('[...') && seg.endsWith(']')) return '*'
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
