// node:util polyfill — inspect, format, promisify, and common utilities.

export function inspect(obj: any, _opts?: any): string {
  try { return JSON.stringify(obj, null, 2); }
  catch { return String(obj); }
}

export function format(fmt: any, ...args: any[]): string {
  if (typeof fmt !== "string") return [fmt, ...args].map(String).join(" ");
  let i = 0;
  const str = fmt.replace(/%[sdifjoO%]/g, (match: string) => {
    if (match === "%%") return "%";
    if (i >= args.length) return match;
    const arg = args[i++];
    switch (match) {
      case "%s": return String(arg);
      case "%d": case "%i": return Number(arg).toString();
      case "%f": return parseFloat(arg).toString();
      case "%j": case "%o": case "%O": try { return JSON.stringify(arg); } catch { return "[Circular]"; }
      default: return match;
    }
  });
  const rest = args.slice(i).map(String).join(" ");
  return rest ? str + " " + rest : str;
}

export function formatWithOptions(_opts: any, fmt: any, ...args: any[]): string {
  return format(fmt, ...args);
}

export function promisify<T>(fn: (...args: any[]) => void): (...args: any[]) => Promise<T> {
  return (...args: any[]) => new Promise((resolve, reject) => {
    fn(...args, (err: any, result: T) => err ? reject(err) : resolve(result));
  });
}

export function callbackify(fn: (...args: any[]) => Promise<any>): (...args: any[]) => void {
  return (...args: any[]) => {
    const cb = args.pop();
    fn(...args).then((r: any) => cb(null, r), (e: any) => cb(e));
  };
}

export function deprecate<F extends (...args: any[]) => any>(fn: F, _msg: string): F { return fn; }
export function inherits(ctor: any, superCtor: any): void { Object.setPrototypeOf(ctor.prototype, superCtor.prototype); }
export function debuglog(_section: string): (...args: any[]) => void { return () => {}; }
export function debug(_section: string): (...args: any[]) => void { return () => {}; }

export const types = {
  isDate: (v: any): v is Date => v instanceof Date,
  isRegExp: (v: any): v is RegExp => v instanceof RegExp,
  isArray: Array.isArray,
  isArrayBuffer: (v: any): v is ArrayBuffer => v instanceof ArrayBuffer,
  isTypedArray: (v: any) => ArrayBuffer.isView(v) && !(v instanceof DataView),
  isPromise: (v: any): v is Promise<any> => v instanceof Promise,
  isMap: (v: any): v is Map<any, any> => v instanceof Map,
  isSet: (v: any): v is Set<any> => v instanceof Set,
};

export const TextDecoder = globalThis.TextDecoder;
export const TextEncoder = globalThis.TextEncoder;

export default {
  inspect, format, formatWithOptions, promisify, callbackify,
  deprecate, inherits, debuglog, debug, types, TextDecoder, TextEncoder,
};
