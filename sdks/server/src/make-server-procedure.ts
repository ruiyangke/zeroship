//
// `__makeServerProcedure(impl, meta)` — server-side counterpart to the
// client procedure brander in `@zeroship/rpc`. The vite-plugin uses
// it while transforming server modules so procedure exports carry the
// same metadata as client stubs.

import type { ProcedureKind } from "./types";

/** Per-procedure metadata mirrored from the client adapter. */
export interface ServerProcedureMeta {
  /** Wire id. */
  id: string;
  /** Discriminator. */
  kind: ProcedureKind;
  /** Wire format. Defaults to `"json"`. */
  wire?: string;
}

/**
 * Wrap a server procedure implementation in a callable procedure
 * reference. The wrapper carries only metadata; React apps should use
 * TanStack Query directly around the callable import.
 */
export function __makeServerProcedure<TIn = unknown, TOut = unknown>(
  impl: (input: TIn) => Promise<TOut> | AsyncIterable<TOut>,
  meta: ServerProcedureMeta,
): ServerProcedureFn<TIn, TOut> {
  const fn = function (input: TIn): Promise<TOut> | AsyncIterable<TOut> {
    return impl(input);
  } as ServerProcedureFn<TIn, TOut>;

  Object.defineProperty(fn, "id", { value: meta.id, enumerable: true });
  Object.defineProperty(fn, "kind", { value: meta.kind, enumerable: true });
  Object.defineProperty(fn, "wire", {
    value: meta.wire ?? "json",
    enumerable: true,
  });

  return fn;
}

export interface ServerProcedureFn<TIn, TOut> {
  (input: TIn): Promise<TOut> | AsyncIterable<TOut>;
  id: string;
  kind: ProcedureKind;
  wire: string;
}

export type ServerQueryProcedure<TIn, TOut> = ServerProcedureFn<TIn, TOut>;
export type ServerMutationProcedure<TIn, TOut> = ServerProcedureFn<TIn, TOut>;
export type ServerStreamProcedure<TIn, TOut> = ServerProcedureFn<TIn, TOut>;
export type ServerSubscriptionProcedure<TIn, TOut> = ServerProcedureFn<TIn, TOut>;
export type ServerActionProcedure<TIn, TOut> = ServerProcedureFn<TIn, TOut>;
