// node:stream polyfill — minimal Readable/Writable/Duplex stubs + web streams.

import { EventEmitter } from "./events.js";

export class Readable extends EventEmitter {
  readable = true;
  readableEnded = false;
  readableFlowing: boolean | null = null;
  constructor(_opts?: any) { super(); }
  pipe<T extends Writable>(dest: T): T { return dest; }
  read(_size?: number): any { return null; }
  destroy(): this { this.readable = false; return this; }
  push(_chunk: any): boolean { return false; }
  unpipe(_dest?: any): this { return this; }
  setEncoding(_enc: string): this { return this; }
  resume(): this { return this; }
  pause(): this { return this; }
}

export class Writable extends EventEmitter {
  writable = true;
  writableEnded = false;
  constructor(_opts?: any) { super(); }
  write(_chunk: any, _encoding?: string, _cb?: (err?: Error) => void): boolean { return true; }
  end(_chunk?: any, _encoding?: string, _cb?: () => void): this { this.writableEnded = true; return this; }
  destroy(): this { this.writable = false; return this; }
}

export class Duplex extends EventEmitter {
  readable = true;
  writable = true;
  constructor(_opts?: any) { super(); }
  pipe<T extends Writable>(dest: T): T { return dest; }
  read(_size?: number): any { return null; }
  write(_chunk: any): boolean { return true; }
  end(): this { return this; }
  destroy(): this { return this; }
}

export class Transform extends Duplex {
  constructor(_opts?: any) { super(_opts); }
}

export class PassThrough extends Transform {}

// Web streams (already in runtime)
export const _ReadableStream = globalThis.ReadableStream;

export const Stream = Readable;

export { EventEmitter };
export default {
  Readable, Writable, Duplex, Transform, PassThrough, Stream, EventEmitter,
  _ReadableStream,
};
