// node:timers/promises polyfill.

export function setTimeout(ms?: number, value?: any): Promise<any> {
  return new Promise(resolve => globalThis.setTimeout(() => resolve(value), ms ?? 0));
}

export function setInterval(_ms?: number, _value?: any): AsyncIterable<any> {
  throw new Error("timers/promises.setInterval is not implemented");
}

export function setImmediate(value?: any): Promise<any> {
  return Promise.resolve(value);
}

export default { setTimeout, setInterval, setImmediate };
