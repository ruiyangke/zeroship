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
} else {
  console.log('Usage: appbase dev [file.jsx|directory] [--port=3000]')
}
