#!/usr/bin/env node
import { dev, devDirectory } from '../src/dev.js'
import { statSync } from 'node:fs'

const [,, command, ...args] = process.argv

if (command === 'dev') {
  const entry = args[0] || 'app'
  const port = parseInt(args.find(a => a.startsWith('--port='))?.split('=')[1] || '3000')

  const stat = statSync(entry, { throwIfNoEntry: false })
  if (stat?.isDirectory()) {
    devDirectory(entry, { port })
  } else {
    dev(entry, { port })
  }
} else if (command === 'test') {
  const entry = args[0] || 'app'
  const { runTests } = await import('../src/test.js')
  const { failed } = await runTests(entry)
  process.exit(failed > 0 ? 1 : 0)
} else {
  console.log('Usage:')
  console.log('  appbase dev [file.jsx|directory] [--port=3000]')
  console.log('  appbase test [file.jsx|directory]')
}
