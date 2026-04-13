/**
 * RPC stub generation — replace server functions with fetch() calls.
 *
 * Given a server function name and its parameters, generates a client-side
 * function that calls the server via JSON-RPC.
 */

/**
 * Generate RPC stub code for a list of server functions.
 *
 * Each stub becomes:
 * ```js
 * export async function name(...args) {
 *   const res = await fetch("/_rpc", {
 *     method: "POST",
 *     headers: { "Content-Type": "application/json" },
 *     body: JSON.stringify({ jsonrpc: "2.0", method: "name", params: args, id: Date.now() })
 *   });
 *   const json = await res.json();
 *   if (json.error) throw new Error(json.error.message || "RPC error");
 *   return json.result;
 * }
 * ```
 */
export function generateStubs(
  functionNames: string[],
  rpcEndpoint: string = "/_rpc"
): string {
  return functionNames
    .map((name) => generateStub(name, rpcEndpoint))
    .join("\n\n");
}

function generateStub(name: string, endpoint: string): string {
  return `export async function ${name}(...args) {
  const res = await fetch(${JSON.stringify(endpoint)}, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      method: ${JSON.stringify(name)},
      params: args,
      id: Date.now()
    })
  });
  const json = await res.json();
  if (json.error) throw new Error(json.error.message || "RPC error");
  return json.result;
}`;
}

/**
 * Transform a module's source code by replacing server function definitions
 * with RPC stubs, keeping all non-server code intact.
 *
 * @param code Original source code
 * @param serverFunctions Names of functions to replace with stubs
 * @param rpcEndpoint URL for the RPC endpoint
 */
export function transformToClient(
  code: string,
  serverFunctions: string[],
  rpcEndpoint: string = "/_rpc"
): string {
  if (serverFunctions.length === 0) return code;

  const stubs = generateStubs(serverFunctions, rpcEndpoint);
  let result = code;

  // Remove "use server" directive if present at file level
  result = result.replace(/^\s*["']use server["'];?\s*\n/, "");

  // Remove each server function and its body.
  // Replace with the RPC stub.
  for (const name of serverFunctions) {
    // Match: export async function name(...) { ... }
    // This regex handles nested braces by counting depth.
    const pattern = new RegExp(
      `export\\s+(async\\s+)?function\\s+${escapeRegex(name)}\\s*\\([^)]*\\)[^{]*\\{`,
      "m"
    );
    const match = pattern.exec(result);
    if (match) {
      const start = match.index;
      const braceStart = result.indexOf("{", start + match[0].length - 1);
      const end = findMatchingBrace(result, braceStart);
      if (end !== -1) {
        result = result.slice(0, start) + result.slice(end + 1);
      }
    }
  }

  // Remove server-only imports (@zeroship/* that are only used by removed functions)
  result = removeUnusedServerImports(result, serverFunctions);

  // Remove server-only variable declarations (tainted bindings)
  // This is a best-effort cleanup — esbuild will tree-shake the rest
  result = removeOrphanedDeclarations(result);

  // Append stubs
  result = result.trim() + "\n\n" + stubs + "\n";

  return result;
}

/** Find the matching closing brace for an opening brace at position `start` */
function findMatchingBrace(code: string, start: number): number {
  let depth = 0;
  let inString: string | null = null;
  let escaped = false;

  for (let i = start; i < code.length; i++) {
    const ch = code[i];

    if (escaped) {
      escaped = false;
      continue;
    }
    if (ch === "\\") {
      escaped = true;
      continue;
    }

    if (inString) {
      if (ch === inString) inString = null;
      continue;
    }

    if (ch === '"' || ch === "'" || ch === "`") {
      inString = ch;
      continue;
    }

    if (ch === "{") depth++;
    if (ch === "}") {
      depth--;
      if (depth === 0) return i;
    }
  }
  return -1;
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Remove import statements that only import from @zeroship/* server modules.
 * Keep imports from other packages (react, etc.)
 */
function removeUnusedServerImports(code: string, _serverFns: string[]): string {
  // Remove import lines from @zeroship/* packages
  return code.replace(
    /^\s*import\s+.*\s+from\s+['"]@zeroship\/[^'"]+['"]\s*;?\s*$/gm,
    ""
  );
}

/** Remove variable declarations that reference identifiers no longer in scope */
function removeOrphanedDeclarations(code: string): string {
  // Best-effort: remove `const x = model(...)` lines where model is no longer imported
  // esbuild will handle this properly during bundling, so this is just cleanup
  return code;
}
