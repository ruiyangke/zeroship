// node:timers polyfill — wraps global timer functions.

export const _setTimeout = globalThis.setTimeout;
export const _clearTimeout = globalThis.clearTimeout;
export const _setInterval = globalThis.setInterval;
export const _clearInterval = globalThis.clearInterval;
export const setImmediate = (fn: (...args: any[]) => void, ...args: any[]) => globalThis.setTimeout(() => fn(...args), 0);
export const clearImmediate = globalThis.clearTimeout;

export { _setTimeout as setTimeout, _clearTimeout as clearTimeout, _setInterval as setInterval, _clearInterval as clearInterval };
export default { setTimeout: _setTimeout, clearTimeout: _clearTimeout, setInterval: _setInterval, clearInterval: _clearInterval, setImmediate, clearImmediate };
