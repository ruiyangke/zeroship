// This module is the compile-time import target.
// At runtime, server code gets the real db, client code gets RPC stubs.
// This file is only used as a marker for the compiler.
export { createDb as db } from './db.js'
export function serve() {
  throw new Error('serve() should not be called directly. Use `appbase dev` to run your app.')
}
