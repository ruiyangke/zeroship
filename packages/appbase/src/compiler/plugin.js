import { parse } from '@babel/parser'
import traverse from '@babel/traverse'
import generate from '@babel/generator'
import * as t from '@babel/types'

export function compile(source, options = {}) {
  const target = options.target || 'node'

  const ast = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  const taintedBindings = new Set()
  const serverFunctions = new Set()
  const exportedServerFns = []
  let entryComponent = null
  let hasFileDirective = false

  // --- Pass 1: Detect taint sources, file directive, serve() ---
  traverse.default(ast, {
    Program(path) {
      const directives = path.node.directives || []
      if (directives.some(d => d.value.value === 'use server')) {
        hasFileDirective = true
      }
    },
    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        for (const spec of path.node.specifiers) {
          const name = spec.local.name
          if (name !== 'serve') {
            taintedBindings.add(name)
          }
        }
      }
    },
    CallExpression(path) {
      if (
        t.isIdentifier(path.node.callee) &&
        path.node.callee.name === 'serve' &&
        path.node.arguments.length > 0 &&
        t.isIdentifier(path.node.arguments[0])
      ) {
        entryComponent = path.node.arguments[0].name
      }
    }
  })

  // --- Pass 2: Propagate taint to direct bindings ---
  traverse.default(ast, {
    VariableDeclarator(path) {
      const init = path.node.init
      if (!init || !t.isIdentifier(path.node.id)) return

      let sourceIdent = null
      if (t.isCallExpression(init)) {
        if (t.isIdentifier(init.callee)) {
          sourceIdent = init.callee.name
        } else if (t.isMemberExpression(init.callee) && t.isIdentifier(init.callee.object)) {
          sourceIdent = init.callee.object.name
        }
      } else if (t.isMemberExpression(init) && t.isIdentifier(init.object)) {
        sourceIdent = init.object.name
      } else if (t.isIdentifier(init)) {
        sourceIdent = init.name
      }

      if (sourceIdent && taintedBindings.has(sourceIdent)) {
        taintedBindings.add(path.node.id.name)
      }
    }
  })

  // --- Pass 3: Detect server functions ---
  function checkFunction(path) {
    const name = path.node.id?.name
    if (!name) return

    const isExported = t.isExportNamedDeclaration(path.parent)

    // Check "use server" directive in body (Babel parses these as Directive nodes)
    const directives = path.node.body?.directives || []
    const hasDirective = directives.some(d => d.value.value === 'use server')

    // Check direct tainted reference
    let hasTaintedRef = false
    path.traverse({
      Identifier(innerPath) {
        if (taintedBindings.has(innerPath.node.name) && !innerPath.isBindingIdentifier()) {
          hasTaintedRef = true
        }
      }
    })

    const isFileLevelServer = hasFileDirective && isExported

    if (hasDirective || hasTaintedRef || isFileLevelServer) {
      serverFunctions.add(name)
      if (isExported || hasDirective) {
        if (!exportedServerFns.includes(name)) {
          exportedServerFns.push(name)
        }
      }
    }
  }

  traverse.default(ast, {
    FunctionDeclaration: checkFunction
  })

  // --- Pass 4a: Generate server code ---
  const serverAst = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  // Helper: strip "use server" directive from function body
  function stripDirective(node) {
    if (node.body?.directives) {
      node.body.directives = node.body.directives.filter(d => d.value.value !== 'use server')
    }
  }

  traverse.default(serverAst, {
    Program(path) {
      // Remove file-level "use server"
      if (path.node.directives) {
        path.node.directives = path.node.directives.filter(d => d.value.value !== 'use server')
      }
    },

    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        if (target === 'rust') {
          path.remove()
        } else {
          path.node.specifiers = path.node.specifiers.filter(
            s => s.local.name !== 'serve'
          )
          if (path.node.specifiers.length === 0) {
            path.remove()
          }
        }
      }
    },

    ExportNamedDeclaration(path) {
      const decl = path.node.declaration
      if (t.isFunctionDeclaration(decl)) {
        if (!serverFunctions.has(decl.id.name)) {
          path.remove()
        } else if (target === 'rust') {
          stripDirective(decl)
          path.replaceWith(decl)
        } else {
          stripDirective(decl)
        }
      }
    },

    FunctionDeclaration(path) {
      if (t.isExportNamedDeclaration(path.parent)) return
      const name = path.node.id?.name
      if (!name || !serverFunctions.has(name)) {
        path.remove()
      } else {
        stripDirective(path.node)
      }
    },

    VariableDeclaration(path) {
      if (t.isExportNamedDeclaration(path.parent)) return
      const decl = path.node.declarations[0]
      if (!decl || !t.isIdentifier(decl.id)) return
      if (!taintedBindings.has(decl.id.name)) {
        path.remove()
      }
    },

    ExpressionStatement(path) {
      if (
        t.isCallExpression(path.node.expression) &&
        t.isIdentifier(path.node.expression.callee) &&
        path.node.expression.callee.name === 'serve'
      ) {
        path.remove()
      }
    },

    // Remove TS types from server output
    TSInterfaceDeclaration(path) { path.remove() },
    TSTypeAliasDeclaration(path) { path.remove() },
  })

  let server = generate.default(serverAst).code.trim()

  if (target === 'rust' && exportedServerFns.length > 0) {
    server += `\n\nglobalThis.__rpc = { ${exportedServerFns.join(', ')} }`
  }

  if (serverFunctions.size === 0) {
    server = ''
  }

  // --- Pass 4b: Generate client code ---
  const clientAst = parse(source, {
    sourceType: 'module',
    plugins: ['jsx', 'typescript']
  })

  traverse.default(clientAst, {
    Program(path) {
      if (path.node.directives) {
        path.node.directives = path.node.directives.filter(d => d.value.value !== 'use server')
      }
    },

    ImportDeclaration(path) {
      if (path.node.source.value === 'appbase') {
        path.remove()
      }
    },

    ExpressionStatement(path) {
      if (
        t.isCallExpression(path.node.expression) &&
        t.isIdentifier(path.node.expression.callee) &&
        path.node.expression.callee.name === 'serve'
      ) {
        path.remove()
      }
    },

    VariableDeclaration(path) {
      if (t.isExportNamedDeclaration(path.parent)) return
      const decl = path.node.declarations[0]
      if (decl && t.isIdentifier(decl.id) && taintedBindings.has(decl.id.name)) {
        path.remove()
      }
    },

    ExportNamedDeclaration(path) {
      const decl = path.node.declaration
      if (t.isFunctionDeclaration(decl) && serverFunctions.has(decl.id.name)) {
        // Exported server function -> RPC stub
        if (!exportedServerFns.includes(decl.id.name)) {
          // Not an RPC endpoint, remove entirely
          path.remove()
          return
        }
        const name = decl.id.name
        const params = decl.params.map(p => {
          if (t.isIdentifier(p)) return p.name
          if (t.isAssignmentPattern(p) && t.isIdentifier(p.left)) return p.left.name
          return '_'
        })
        const rpcStub = buildRpcStub(name, params)
        rpcStub.__rpcReplaced = true
        path.replaceWith(rpcStub)
        path.skip()
      }
    },

    FunctionDeclaration(path) {
      if (path.node.__rpcReplaced) return
      if (t.isExportNamedDeclaration(path.parent)) return
      const name = path.node.id?.name
      if (name && serverFunctions.has(name)) {
        // Non-exported server function: remove from client
        path.remove()
      } else if (path.node.body?.directives) {
        // Strip "use server" from remaining functions (shouldn't happen but safety)
        path.node.body.directives = path.node.body.directives.filter(d => d.value.value !== 'use server')
      }
    },
  })

  const client = generate.default(clientAst).code

  return {
    server,
    client,
    entryComponent,
    serverFunctions: exportedServerFns
  }
}

function buildRpcStub(name, params) {
  const paramList = params.join(', ')
  const code = `
    async function ${name}(${paramList}) {
      const res = await fetch('/rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: '${name}',
          params: [${paramList}],
          id: Date.now()
        })
      });
      const data = await res.json();
      if (data.error) throw new Error(data.error.message);
      return data.result;
    }
  `
  return parse(code, { sourceType: 'module' }).program.body[0]
}
