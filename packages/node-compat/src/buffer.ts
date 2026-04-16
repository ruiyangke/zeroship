// node:buffer polyfill — Buffer class extending Uint8Array.

function toHex(bytes: Uint8Array): string {
  let hex = "";
  for (let i = 0; i < bytes.length; i++) {
    hex += (bytes[i] >> 4).toString(16) + (bytes[i] & 0xf).toString(16);
  }
  return hex;
}

// @ts-ignore — Buffer.from signature differs from Uint8Array.from
export class Buffer extends Uint8Array {
  static from(input: any, encoding?: string): Buffer {
    if (typeof input === "string") {
      if (encoding === "base64" || encoding === "base64url") {
        const str = encoding === "base64url"
          ? input.replace(/-/g, "+").replace(/_/g, "/")
          : input;
        const binary = atob(str);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        return new Buffer(bytes.buffer);
      }
      if (encoding === "hex") {
        const matches = input.match(/.{1,2}/g) || [];
        return new Buffer(new Uint8Array(matches.map((b: string) => parseInt(b, 16))).buffer);
      }
      // Default: utf-8
      return new Buffer(new TextEncoder().encode(input).buffer);
    }
    if (input instanceof ArrayBuffer) return new Buffer(input);
    if (ArrayBuffer.isView(input)) return new Buffer(input.buffer as ArrayBuffer, input.byteOffset, input.byteLength);
    if (Array.isArray(input)) return new Buffer(new Uint8Array(input).buffer);
    return new Buffer(0);
  }

  static alloc(size: number, fill?: number | string): Buffer {
    const buf = new Buffer(size);
    if (fill !== undefined) buf.fill(typeof fill === "string" ? fill.charCodeAt(0) : fill);
    return buf;
  }

  static allocUnsafe(size: number): Buffer { return new Buffer(size); }
  static allocUnsafeSlow(size: number): Buffer { return new Buffer(size); }
  static isBuffer(obj: any): obj is Buffer { return obj instanceof Buffer; }
  static isEncoding(enc: string): boolean { return ["utf8", "utf-8", "hex", "base64", "base64url", "ascii", "binary", "latin1"].includes(enc); }

  static concat(list: (Buffer | Uint8Array)[], length?: number): Buffer {
    if (!length) length = list.reduce((s, b) => s + b.length, 0);
    const result = Buffer.alloc(length);
    let offset = 0;
    for (const buf of list) {
      result.set(buf, offset);
      offset += buf.length;
    }
    return result;
  }

  static byteLength(str: string | ArrayBufferView, encoding?: string): number {
    if (typeof str !== "string") return (str as any).length ?? (str as any).byteLength ?? 0;
    if (encoding === "hex") return str.length / 2;
    if (encoding === "base64" || encoding === "base64url") return Math.ceil(str.length * 3 / 4);
    return new TextEncoder().encode(str).length;
  }

  toString(encoding?: string, start?: number, end?: number): string {
    const slice = start !== undefined || end !== undefined
      ? this.subarray(start ?? 0, end ?? this.length)
      : this;
    if (encoding === "hex") return toHex(slice);
    if (encoding === "base64") return btoa(String.fromCharCode(...slice));
    if (encoding === "base64url") return btoa(String.fromCharCode(...slice)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    return new TextDecoder().decode(slice);
  }

  toJSON(): { type: string; data: number[] } {
    return { type: "Buffer", data: Array.from(this) };
  }

  write(str: string, offset?: number, length?: number, _encoding?: string): number {
    const bytes = new TextEncoder().encode(str);
    const len = Math.min(bytes.length, length ?? bytes.length, this.length - (offset ?? 0));
    this.set(bytes.subarray(0, len), offset ?? 0);
    return len;
  }

  equals(other: Uint8Array): boolean {
    if (this.length !== other.length) return false;
    for (let i = 0; i < this.length; i++) if (this[i] !== other[i]) return false;
    return true;
  }

  compare(other: Uint8Array): number {
    const len = Math.min(this.length, other.length);
    for (let i = 0; i < len; i++) {
      if (this[i] < other[i]) return -1;
      if (this[i] > other[i]) return 1;
    }
    return this.length - other.length;
  }

  copy(target: Uint8Array, targetStart = 0, sourceStart = 0, sourceEnd = this.length): number {
    const slice = this.subarray(sourceStart, sourceEnd);
    target.set(slice, targetStart);
    return slice.length;
  }

  readUInt8(offset: number): number { return this[offset]; }
  readUInt16BE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getUint16(offset, false); }
  readUInt16LE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getUint16(offset, true); }
  readUInt32BE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getUint32(offset, false); }
  readUInt32LE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getUint32(offset, true); }
  readInt8(offset: number): number { return new DataView(this.buffer, this.byteOffset).getInt8(offset); }
  readInt16BE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getInt16(offset, false); }
  readInt16LE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getInt16(offset, true); }
  readInt32BE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getInt32(offset, false); }
  readInt32LE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getInt32(offset, true); }
  readFloatBE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getFloat32(offset, false); }
  readFloatLE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getFloat32(offset, true); }
  readDoubleBE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getFloat64(offset, false); }
  readDoubleLE(offset: number): number { return new DataView(this.buffer, this.byteOffset).getFloat64(offset, true); }

  writeUInt8(value: number, offset: number): number { this[offset] = value; return offset + 1; }
  writeUInt16BE(value: number, offset: number): number { new DataView(this.buffer, this.byteOffset).setUint16(offset, value, false); return offset + 2; }
  writeUInt16LE(value: number, offset: number): number { new DataView(this.buffer, this.byteOffset).setUint16(offset, value, true); return offset + 2; }
  writeUInt32BE(value: number, offset: number): number { new DataView(this.buffer, this.byteOffset).setUint32(offset, value, false); return offset + 4; }
  writeUInt32LE(value: number, offset: number): number { new DataView(this.buffer, this.byteOffset).setUint32(offset, value, true); return offset + 4; }
}

// Make Buffer globally available
(globalThis as any).Buffer = Buffer;

export default { Buffer };
