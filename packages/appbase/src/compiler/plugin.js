import { parse } from '@babel/parser'
import traverse from '@babel/traverse'
import generate from '@babel/generator'
import * as t from '@babel/types'

export function compile(source) {
  const ast = parse(source, {
    sourceType: 'module',
    plugins: ['jsx']
  })

  // Track which identifiers come from 'appbase' server imports
  const serverImports = new Set() // e.g. 'db'
  const serverBindings = new Set() // e.g. 'todos' (from db.collection)
  const serverFunctions = new Set() // e.g. 'addTodo', 'getTodos'
  let entryComponent = null

  // Pass 1: Find appbase imports and serve() call
  traverse.default(ast, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        for (const spec of path.node.specifiers) {
          if (spec.imported.name !== 'serve') {
            serverImports.add(spec.local.name)
          }
        }
      }
    },
    CallExpression(path) {
      if (path.node.callee.name === 'serve' && path.node.arguments.length > 0) {
        entryComponent = path.node.arguments[0].name
      }
    }
  })

  // Pass 2: Find bindings that use server imports (e.g. const todos = db.collection(...))
  traverse.default(ast, {
    VariableDeclarator(path) {
      const init = path.node.init
      if (
        init &&
        t.isCallExpression(init) &&
        t.isMemberExpression(init.callee) &&
        t.isIdentifier(init.callee.object) &&
        serverImports.has(init.callee.object.name)
      ) {
        serverBindings.add(path.node.id.name)
      }
    }
  })

  // Pass 3: Find functions that reference server bindings
  traverse.default(ast, {
    'FunctionDeclaration|FunctionExpression'(path) {
      const name = path.node.id?.name
      if (!name) return

      let usesServer = false
      path.traverse({
        Identifier(innerPath) {
          if (serverBindings.has(innerPath.node.name)) {
            usesServer = true
          }
        }
      })

      if (usesServer) {
        serverFunctions.add(name)
      }
    }
  })

  // Generate server code: keep imports, server bindings, server functions
  const serverAst = parse(source, { sourceType: 'module', plugins: ['jsx'] })

  traverse.default(serverAst, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        // Keep only server imports (remove 'serve')
        path.node.specifiers = path.node.specifiers.filter(
          s => s.imported.name !== 'serve'
        )
        if (path.node.specifiers.length === 0) {
          path.remove()
        }
      }
    },
    FunctionDeclaration(path) {
      if (!serverFunctions.has(path.node.id.name)) {
        path.remove()
      }
    },
    ExpressionStatement(path) {
      if (t.isCallExpression(path.node.expression) &&
          t.isIdentifier(path.node.expression.callee) &&
          path.node.expression.callee.name === 'serve') {
        path.remove()
      }
    }
  })

  const server = generate.default(serverAst).code

  // Generate client code: remove server-only stuff, rewrite function calls to fetch
  const clientAst = parse(source, { sourceType: 'module', plugins: ['jsx'] })

  traverse.default(clientAst, {
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        // Remove all appbase imports from client (both server primitives and serve)
        path.remove()
      }
    },
    ExpressionStatement(path) {
      // Remove serve() call from client code
      if (t.isCallExpression(path.node.expression) &&
          t.isIdentifier(path.node.expression.callee) &&
          path.node.expression.callee.name === 'serve') {
        path.remove()
      }
    },
    VariableDeclaration(path) {
      // Remove server bindings (const todos = db.collection(...))
      const decl = path.node.declarations[0]
      if (decl && t.isIdentifier(decl.id) && serverBindings.has(decl.id.name)) {
        path.remove()
      }
    },
    FunctionDeclaration(path) {
      if (serverFunctions.has(path.node.id.name)) {
        // Replace server function with RPC stub
        const name = path.node.id.name
        const params = path.node.params.map(p => p.name)
        const rpcFn = parse(`
          async function ${name}(${params.join(', ')}) {
            const res = await fetch('/api/${name}', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ args: [${params.join(', ')}] })
            });
            return res.json();
          }
        `, { sourceType: 'module' }).program.body[0]
        path.replaceWith(rpcFn)
        path.skip()
      }
    }
  })

  const client = generate.default(clientAst).code

  return { server, client, entryComponent }
}
