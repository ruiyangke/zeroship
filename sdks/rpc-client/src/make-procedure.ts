//
// `__makeProcedure(call, meta)` wraps a raw RPC call in a callable
// procedure reference. The result stays intentionally small: app code
// calls it like a normal async function, and framework code can inspect
// its stable metadata and server-reference brand.

/** Procedure kind — matches the wire protocol's discriminator. */
export type ProcedureKind = "query" | "mutation" | "stream" | "subscription";

/**
 * Per-procedure metadata. The build-time transform injects this; users
 * supplying procedures by hand pass it explicitly.
 */
export interface ProcedureBuildMeta {
  /** Wire id — stable across refactors. */
  id: string;
  /** Discriminator. */
  kind: ProcedureKind;
  /**
   * Wire format the stub speaks. `"json"` is the only shipped format
   * today; the field is reserved so future wire variants can be tagged
   * at build time without a meta shape change. Defaults to `"json"`.
   */
  wire?: string;
}

/**
 * Brand symbol applied to every `__makeProcedure` return value. RSC-style
 * `<form action={fn}>` and prop-passed server actions detect server
 * references at runtime by reading this property; the
 * `Symbol.for("zeroship/server-reference")` registration makes the
 * symbol survive realm boundaries.
 */
export const __SERVER_REFERENCE: symbol = Symbol.for(
  "zeroship/server-reference",
);

/**
 * Caller signature. Always async for query/mutation procedures; stream
 * procedures return the AsyncIterable directly.
 */
export type ProcedureCaller<TIn, TOut> = (
  input: TIn,
) => Promise<TOut> | AsyncIterable<TOut>;

export interface ProcedureFn<TIn, TOut> {
  (input: TIn): Promise<TOut> | AsyncIterable<TOut>;
  id: string;
  kind: ProcedureKind;
  wire: string;
}

export type QueryProcedure<TIn, TOut> = ProcedureFn<TIn, TOut>;
export type MutationProcedure<TIn, TOut> = ProcedureFn<TIn, TOut>;
export type StreamProcedure<TIn, TOut> = ProcedureFn<TIn, TOut>;
export type SubscriptionProcedure<TIn, TOut> = ProcedureFn<TIn, TOut>;

/**
 * Wrap a raw RPC call into a callable procedure reference.
 *
 * `call` is the closure that performs the underlying request and returns
 * the procedure output. The returned object is callable and carries only
 * build/runtime metadata: `id`, `kind`, `wire`, and the server-reference
 * brand.
 */
export function __makeProcedure<TIn = unknown, TOut = unknown>(
  call: ProcedureCaller<TIn, TOut>,
  meta: ProcedureBuildMeta,
): ProcedureFn<TIn, TOut> {
  const fn = function (input: TIn): Promise<TOut> | AsyncIterable<TOut> {
    return call(input);
  } as ProcedureFn<TIn, TOut>;

  Object.defineProperty(fn, "id", { value: meta.id, enumerable: true });
  Object.defineProperty(fn, "kind", { value: meta.kind, enumerable: true });
  Object.defineProperty(fn, "wire", {
    value: meta.wire ?? "json",
    enumerable: true,
  });

  // Server-reference brand — non-enumerable so JSON.stringify and
  // dev-tools enumeration stay clean; Symbol.for lets cross-realm
  // consumers re-derive the same key.
  Object.defineProperty(fn, __SERVER_REFERENCE, {
    value: true,
    enumerable: false,
    configurable: true,
    writable: false,
  });

  return fn;
}
