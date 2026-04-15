// Ambient type declarations for Web APIs available in the zeroship V8 runtime.
// These are NOT polyfills — they describe what the runtime already provides.

declare const crypto: {
  randomUUID(): string;
  getRandomValues<T extends ArrayBufferView>(buf: T): T;
  subtle: {
    digest(algorithm: string, data: ArrayBuffer | ArrayBufferView): Promise<ArrayBuffer>;
    importKey(...args: any[]): Promise<CryptoKey>;
    sign(...args: any[]): Promise<ArrayBuffer>;
    verify(...args: any[]): Promise<boolean>;
    encrypt(...args: any[]): Promise<ArrayBuffer>;
    decrypt(...args: any[]): Promise<ArrayBuffer>;
  };
};

declare class CryptoKey {}

declare class TextEncoder {
  encode(input?: string): Uint8Array;
}

declare class TextDecoder {
  decode(input?: ArrayBufferView, options?: { stream?: boolean }): string;
}

declare class ReadableStream<R = any> {
  constructor(source?: any, strategy?: any);
  getReader(): any;
}

declare class WritableStream<W = any> {
  constructor(sink?: any, strategy?: any);
}

declare class TransformStream<I = any, O = any> {
  constructor(transformer?: any, writableStrategy?: any, readableStrategy?: any);
}

declare class Response {
  constructor(body?: any, init?: { status?: number; headers?: Record<string, string> });
  readonly status: number;
  readonly headers: Headers;
}

declare class Headers {
  get(name: string): string | null;
  set(name: string, value: string): void;
}

declare class URL {
  constructor(url: string, base?: string);
  readonly href: string;
  readonly pathname: string;
  readonly hostname: string;
  readonly port: string;
  readonly protocol: string;
  readonly search: string;
  readonly searchParams: URLSearchParams;
}

declare class URLSearchParams {
  constructor(init?: string | Record<string, string>);
  get(name: string): string | null;
  set(name: string, value: string): void;
}

declare function setTimeout(callback: (...args: any[]) => void, ms?: number, ...args: any[]): number;
declare function clearTimeout(id: number): void;
declare function setInterval(callback: (...args: any[]) => void, ms?: number, ...args: any[]): number;
declare function clearInterval(id: number): void;
declare function queueMicrotask(callback: () => void): void;
declare function btoa(data: string): string;
declare function atob(data: string): string;
declare function fetch(url: string, init?: any): Promise<Response>;

declare const console: {
  log(...args: any[]): void;
  warn(...args: any[]): void;
  error(...args: any[]): void;
  info(...args: any[]): void;
};

declare const globalThis: typeof globalThis & {
  process?: { env?: Record<string, string>; version?: string };
  Buffer?: any;
  performance?: { now(): number };
};

declare const performance: { now(): number };
