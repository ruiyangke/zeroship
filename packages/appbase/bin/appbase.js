#!/usr/bin/env node
import { dev } from '../src/dev.js'

const [,, command, ...args] = process.argv

if (command === 'dev') {
  const entry = args[0] || 'app.jsx'
  const port = parseInt(args.find(a => a.startsWith('--port='))?.split('=')[1] || '3000')
  dev(entry, { port })
} else {
  console.log('Usage: appbase dev [file.jsx] [--port=3000]')
}
