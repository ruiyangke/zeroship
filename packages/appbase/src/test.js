import { readFileSync, existsSync, readdirSync } from 'node:fs'
import { resolve, dirname, join } from 'node:path'
import { compile } from './compiler/plugin.js'
import { compileDirectory } from './compiler/directory.js'
import { createDb } from './db.js'
import { statSync } from 'node:fs'

// Colors for terminal output
const green = (s) => `\x1b[32m${s}\x1b[0m`
const red = (s) => `\x1b[31m${s}\x1b[0m`
const dim = (s) => `\x1b[2m${s}\x1b[0m`
const bold = (s) => `\x1b[1m${s}\x1b[0m`

/**
 * Load server functions from compiled code into callable functions.
 * Uses an in-memory SQLite database for isolation.
 */
export function loadServerFunctions(serverCode, dbInstance) {
  const db = dbInstance || createDb(':memory:')

  // Strip ES module syntax and eval with db in scope
  const cleaned = serverCode
    .replace(/import\s*\{[^}]*\}\s*from\s*['"][^'"]*['"];?/g, '')
    .replace(/export\s+/g, '')

  const fnNames = Array.from(cleaned.matchAll(/async\s+function\s+(\w+)/g)).map(m => m[1])

  const wrapper = `
    ${cleaned}
    return { ${fnNames.join(', ')} }
  `

  const factory = new Function('db', wrapper)
  const functions = factory(db)

  return { functions, db, fnNames }
}

/**
 * Create an RPC helper that calls server functions directly (no HTTP).
 */
function createRpcHelper(functions) {
  return async function rpc(method, params = []) {
    const fn = functions[method]
    if (!fn) throw new Error(`Server function '${method}' not found`)
    return fn(...params)
  }
}

/**
 * Run auto-generated smoke tests for all server functions.
 * Tests basic CRUD lifecycle if insert/find/delete functions are detected.
 */
async function runAutoTests(functions, fnNames) {
  const results = []

  // Detect CRUD patterns
  const insertFn = fnNames.find(n => /^(add|create|insert)/i.test(n))
  const listFn = fnNames.find(n => /^(get|list|find|fetch)/i.test(n) && !/ById|BySlug/i.test(n))
  const updateFn = fnNames.find(n => /^(update|toggle|edit|patch)/i.test(n))
  const deleteFn = fnNames.find(n => /^(delete|remove)/i.test(n))

  // Test each function doesn't throw with reasonable inputs
  for (const name of fnNames) {
    try {
      let result
      if (name === insertFn) {
        result = await functions[name]('test item')
        results.push({ name: `${name}("test item")`, pass: true, detail: `returned ${typeof result}${result?.id ? ' with id' : ''}` })
      } else if (name === listFn) {
        result = await functions[name]()
        const isArray = Array.isArray(result)
        results.push({ name: `${name}()`, pass: isArray, detail: isArray ? `returned array (${result.length} items)` : `expected array, got ${typeof result}` })
      } else if (name === updateFn && insertFn) {
        // Need an item to update
        const item = await functions[insertFn]('update test')
        if (item?.id) {
          result = await functions[name](item.id, true)
          results.push({ name: `${name}(id, true)`, pass: true, detail: `returned ${typeof result}` })
        } else {
          results.push({ name: `${name}()`, pass: false, detail: 'no item id to test with' })
        }
      } else if (name === deleteFn && insertFn) {
        const item = await functions[insertFn]('delete test')
        if (item?.id) {
          await functions[name](item.id)
          results.push({ name: `${name}(id)`, pass: true, detail: 'no error' })
        } else {
          results.push({ name: `${name}()`, pass: false, detail: 'no item id to test with' })
        }
      } else {
        // Unknown pattern — just call with no args and see if it throws
        try {
          result = await functions[name]()
          results.push({ name: `${name}()`, pass: true, detail: `returned ${typeof result}` })
        } catch (e) {
          // Try with a string arg
          try {
            result = await functions[name]('test')
            results.push({ name: `${name}("test")`, pass: true, detail: `returned ${typeof result}` })
          } catch (e2) {
            results.push({ name: `${name}()`, pass: false, detail: e2.message })
          }
        }
      }
    } catch (e) {
      results.push({ name: `${name}()`, pass: false, detail: e.message })
    }
  }

  // CRUD lifecycle test if we have insert + list + delete
  if (insertFn && listFn) {
    try {
      const before = await functions[listFn]()
      const item = await functions[insertFn]('lifecycle test')
      const after = await functions[listFn]()
      const grew = Array.isArray(after) && after.length === before.length + 1
      results.push({ name: 'CRUD lifecycle: insert increases count', pass: grew, detail: grew ? `${before.length} -> ${after.length}` : 'count did not increase' })

      if (deleteFn && item?.id) {
        await functions[deleteFn](item.id)
        const afterDelete = await functions[listFn]()
        const shrank = Array.isArray(afterDelete) && afterDelete.length === before.length
        results.push({ name: 'CRUD lifecycle: delete decreases count', pass: shrank, detail: shrank ? `${after.length} -> ${afterDelete.length}` : 'count did not decrease' })
      }
    } catch (e) {
      results.push({ name: 'CRUD lifecycle', pass: false, detail: e.message })
    }
  }

  return results
}

/**
 * Run user-written test files.
 * Test files export functions via: export function testName({ rpc, db }) { ... }
 * Or use: import { test } from 'appbase/test'
 */
const userTests = []

export function test(name, fn) {
  userTests.push({ name, fn })
}

async function runUserTests(testFile, serverCode) {
  userTests.length = 0

  // Load the test file
  await import(resolve(testFile) + '?t=' + Date.now())

  const results = []

  for (const { name, fn } of userTests) {
    // Each test gets a fresh DB + fresh functions for isolation
    const { functions: freshFns, db: freshDb } = loadServerFunctions(serverCode)
    const rpc = createRpcHelper(freshFns)
    try {
      await fn({ rpc, db: freshDb, functions: freshFns })
      results.push({ name, pass: true, detail: '' })
    } catch (e) {
      results.push({ name, pass: false, detail: e.message })
    }
    freshDb.close()
  }

  return results
}

/**
 * Find .test.js files near the entry
 */
function findTestFiles(entry) {
  const files = []
  const stat = statSync(entry)

  if (stat.isDirectory()) {
    walkForTests(entry, files)
  } else {
    const dir = dirname(entry)
    const base = entry.replace(/\.(jsx?|tsx?)$/, '')
    const candidates = [`${base}.test.js`, `${base}.test.ts`, join(dir, 'tests')]
    for (const c of candidates) {
      if (existsSync(c)) {
        if (statSync(c).isDirectory()) {
          walkForTests(c, files)
        } else {
          files.push(c)
        }
      }
    }
  }

  return files
}

function walkForTests(dir, files) {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) {
      walkForTests(full, files)
    } else if (entry.endsWith('.test.js') || entry.endsWith('.test.ts')) {
      files.push(full)
    }
  }
}

/**
 * Main test runner.
 */
export async function runTests(entry, options = {}) {
  const stat = statSync(entry)
  let serverCode, serverFunctions

  console.log(bold(`\n[appbase test] ${entry}\n`))

  // Compile
  if (stat.isDirectory()) {
    const result = compileDirectory(resolve(entry), { target: 'node' })
    serverCode = result.serverBundle
    serverFunctions = result.serverFunctions
    console.log(dim(`  Routes: ${result.routes.map(r => r.path).join(', ')}`))
  } else {
    const source = readFileSync(resolve(entry), 'utf-8')
    const result = compile(source, { target: 'node' })
    serverCode = result.server
    serverFunctions = result.serverFunctions
  }

  if (!serverCode || serverFunctions.length === 0) {
    console.log(dim('  No server functions found. Nothing to test.\n'))
    return { passed: 0, failed: 0 }
  }

  console.log(dim(`  Server functions: ${serverFunctions.join(', ')}\n`))

  // Load functions with in-memory DB
  const { functions, db, fnNames } = loadServerFunctions(serverCode)

  let totalPassed = 0
  let totalFailed = 0

  // Auto-generated tests
  console.log(bold('  Auto Tests'))
  const autoResults = await runAutoTests(functions, fnNames)
  for (const r of autoResults) {
    if (r.pass) {
      console.log(`    ${green('\u2713')} ${r.name} ${dim(r.detail)}`)
      totalPassed++
    } else {
      console.log(`    ${red('\u2717')} ${r.name} ${red(r.detail)}`)
      totalFailed++
    }
  }

  // User-written tests
  const testFiles = findTestFiles(entry)
  if (testFiles.length > 0) {
    console.log('')
    console.log(bold('  User Tests'))

    for (const file of testFiles) {
      const results = await runUserTests(file, serverCode)
      for (const r of results) {
        if (r.pass) {
          console.log(`    ${green('\u2713')} ${r.name}`)
          totalPassed++
        } else {
          console.log(`    ${red('\u2717')} ${r.name} ${red(r.detail)}`)
          totalFailed++
        }
      }
    }
  }

  // Summary
  console.log('')
  const summary = `  ${totalPassed + totalFailed} tests: ${green(totalPassed + ' passed')}${totalFailed > 0 ? ', ' + red(totalFailed + ' failed') : ''}`
  console.log(summary)
  console.log('')

  db.close()
  return { passed: totalPassed, failed: totalFailed }
}
