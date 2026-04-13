/**
 * Module analysis — detect "use server" directives and server functions.
 *
 * Uses @swc/core to parse the AST and find:
 * 1. File-level "use server" → entire module is server-only
 * 2. Function-level "use server" → individual functions are server
 * 3. Tainted imports → functions using server-module imports are server
 */

import { parseSync, type Module, type Statement, type ModuleItem, type Expression } from "@swc/core";

export interface AnalysisResult {
  /** True if the file has "use server" at the top level */
  isServerModule: boolean;
  /** Names of exported functions that are server-only */
  serverFunctions: string[];
  /** All exported function names (server + client) */
  allExports: string[];
  /** Module specifiers that this file imports from */
  imports: { source: string; names: string[] }[];
}

/**
 * Analyze a module for "use server" directives and server function detection.
 *
 * @param code Source code (TS/TSX/JS/JSX)
 * @param filename Used for parser syntax detection
 * @param serverModules Set of module specifiers known to be server modules
 */
export function analyzeModule(
  code: string,
  filename: string,
  serverModules: Set<string>
): AnalysisResult {
  const isTsx = filename.endsWith(".tsx") || filename.endsWith(".jsx");

  const ast = parseSync(code, {
    syntax: isTsx ? "typescript" : "typescript",
    tsx: isTsx,
    target: "es2022",
  });

  const result: AnalysisResult = {
    isServerModule: false,
    serverFunctions: [],
    allExports: [],
    imports: [],
  };

  // Check file-level "use server"
  if (ast.body.length > 0) {
    const first = ast.body[0];
    if (
      first.type === "ExpressionStatement" &&
      first.expression.type === "StringLiteral" &&
      first.expression.value === "use server"
    ) {
      result.isServerModule = true;
    }
  }

  // Collect imports
  const taintedBindings = new Set<string>();

  for (const item of ast.body) {
    if (item.type === "ImportDeclaration") {
      const source = item.source.value;
      const names: string[] = [];

      for (const spec of item.specifiers) {
        if (spec.type === "ImportSpecifier") {
          names.push(spec.local.value);
        } else if (spec.type === "ImportDefaultSpecifier") {
          names.push(spec.local.value);
        } else if (spec.type === "ImportNamespaceSpecifier") {
          names.push(spec.local.value);
        }
      }

      result.imports.push({ source, names });

      // If importing from a server module, taint all bindings
      if (serverModules.has(source)) {
        for (const name of names) {
          taintedBindings.add(name);
        }
      }
    }
  }

  // Propagate taint: const x = taintedFn(...) → x is tainted
  for (const item of ast.body) {
    if (item.type === "VariableDeclaration") {
      for (const decl of item.declarations) {
        if (decl.id.type === "Identifier" && decl.init) {
          const source = extractCallSource(decl.init);
          if (source && taintedBindings.has(source)) {
            taintedBindings.add(decl.id.value);
          }
        }
      }
    }
  }

  // Find exported functions and determine if they're server functions
  for (const item of ast.body) {
    // export async function foo() { ... }
    if (item.type === "ExportDeclaration" && item.declaration.type === "FunctionDeclaration") {
      const name = item.declaration.identifier.value;
      result.allExports.push(name);

      if (result.isServerModule) {
        result.serverFunctions.push(name);
      } else if (hasFunctionDirective(item.declaration.body, "use server")) {
        result.serverFunctions.push(name);
      } else if (referencesTainted(item.declaration.body, taintedBindings)) {
        result.serverFunctions.push(name);
      }
    }

    // export default function Foo() { ... }
    if (item.type === "ExportDefaultDeclaration") {
      if (item.decl.type === "FunctionExpression" && item.decl.identifier) {
        result.allExports.push(item.decl.identifier.value);
      }
    }

    // Non-exported functions (for taint tracking)
    if (item.type === "FunctionDeclaration") {
      const name = item.identifier.value;
      if (hasFunctionDirective(item.body, "use server")) {
        taintedBindings.add(name);
      } else if (referencesTainted(item.body, taintedBindings)) {
        taintedBindings.add(name);
      }
    }
  }

  return result;
}

/** Extract the callee identifier from a call expression: `foo(...)` → "foo" */
function extractCallSource(expr: Expression): string | null {
  if (expr.type === "CallExpression") {
    if (expr.callee.type === "Identifier") {
      return expr.callee.value;
    }
    if (expr.callee.type === "MemberExpression" && expr.callee.object.type === "Identifier") {
      return expr.callee.object.value;
    }
  }
  if (expr.type === "Identifier") {
    return expr.value;
  }
  if (expr.type === "MemberExpression" && expr.object.type === "Identifier") {
    return expr.object.value;
  }
  return null;
}

/** Check if a function body starts with "use server" directive */
function hasFunctionDirective(
  body: { stmts: Statement[] } | undefined,
  directive: string
): boolean {
  if (!body || body.stmts.length === 0) return false;
  const first = body.stmts[0];
  return (
    first.type === "ExpressionStatement" &&
    first.expression.type === "StringLiteral" &&
    first.expression.value === directive
  );
}

/** Check if a function body references any tainted bindings (shallow check) */
function referencesTainted(
  body: { stmts: Statement[] } | undefined,
  tainted: Set<string>
): boolean {
  if (!body) return false;
  // Simple approach: serialize the body and check for tainted identifiers.
  // A proper implementation would walk the AST, but for v1 this works.
  const bodyStr = JSON.stringify(body);
  for (const name of tainted) {
    // Look for identifier references: "value":"<name>"
    if (bodyStr.includes(`"value":"${name}"`)) {
      return true;
    }
  }
  return false;
}

/**
 * Check if a file has "use server" as its first statement.
 * Fast path — reads only the first non-empty, non-comment line.
 */
export function isServerModuleQuick(code: string): boolean {
  for (const line of code.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("//") || trimmed.startsWith("/*")) continue;
    return (
      trimmed === '"use server"' ||
      trimmed === '"use server";' ||
      trimmed === "'use server'" ||
      trimmed === "'use server';"
    );
  }
  return false;
}
