// node:async_hooks polyfill — AsyncLocalStorage is the key API.
// Uses closure-based tracking (not actual V8 async hooks).

class AsyncLocalStorage<T = any> {
  private _store: T | undefined = undefined;

  getStore(): T | undefined { return this._store; }

  run<R>(store: T, fn: (...args: any[]) => R, ...args: any[]): R {
    const prev = this._store;
    this._store = store;
    try { return fn(...args); }
    finally { this._store = prev; }
  }

  exit<R>(fn: (...args: any[]) => R, ...args: any[]): R {
    const prev = this._store;
    this._store = undefined;
    try { return fn(...args); }
    finally { this._store = prev; }
  }

  enterWith(store: T): void { this._store = store; }
  disable(): void { this._store = undefined; }

  static bind<F extends (...args: any[]) => any>(fn: F): F { return fn; }
  static snapshot(): <R>(fn: (...args: any[]) => R, ...args: any[]) => R {
    return (fn, ...args) => fn(...args);
  }
}

class AsyncResource {
  type: string;
  constructor(type: string) { this.type = type; }
  runInAsyncScope<R>(fn: (...args: any[]) => R, thisArg?: any, ...args: any[]): R {
    return fn.call(thisArg, ...args);
  }
  emitDestroy(): this { return this; }
  asyncId(): number { return 0; }
  triggerAsyncId(): number { return 0; }
  bind<F extends (...args: any[]) => any>(fn: F): F { return fn; }
  static bind<F extends (...args: any[]) => any>(fn: F): F { return fn; }
}

export function createHook() {
  return { enable() { return this; }, disable() { return this; } };
}
export function executionAsyncId(): number { return 0; }
export function executionAsyncResource(): Record<string, string> { return Object.create(null); }
export function triggerAsyncId(): number { return 0; }

export { AsyncLocalStorage, AsyncResource };
export default { AsyncLocalStorage, AsyncResource, createHook, executionAsyncId, executionAsyncResource, triggerAsyncId };
