// node:stream/web — re-exports global Web Streams API.

export const ReadableStream = globalThis.ReadableStream;
export const WritableStream = (globalThis as any).WritableStream;
export const TransformStream = (globalThis as any).TransformStream;
export default { ReadableStream, WritableStream, TransformStream };
