import type { NativeCollection } from "../../../../packages/db/src/native";
import type { Result, Row, RowId, Actor } from "../../../../packages/db/src/types";

export interface MaskingCollectionInternals<S> {
  _run<T>(fn: () => Promise<T>): Promise<Result<T>>;
  _nativeCollection(): NativeCollection;
  _toColumn(field: string): string;
  _toField(column: string): string;
}

/** Bulk unmask a set of rows atomically through the native collection. */
export function bulkUnmaskCollection<S>(
  self: MaskingCollectionInternals<S>,
  items: ReadonlyArray<{
    id: RowId<S>;
    columns: readonly (string & keyof Row<S>)[];
  }>,
  opts: { actor: Actor; reason?: string },
): Promise<Result<Map<RowId<S>, Record<string, unknown>>>> {
  return self._run(async () => {
    const wireItems = items.map((it) => ({
      rowPk: String(it.id),
      columns: it.columns.map((c) => self._toColumn(c as string)),
    }));
    const result = await self._nativeCollection().bulkUnmask(wireItems, {
      actor: opts.actor,
      reason: opts.reason,
    });
    const out = new Map<RowId<S>, Record<string, unknown>>();
    for (const { id } of items) {
      const cols = result.results?.[String(id)];
      if (!cols) continue;
      const mapped: Record<string, unknown> = {};
      for (const [col, plaintext] of Object.entries(cols)) {
        mapped[self._toField(col)] = plaintext;
      }
      out.set(id, mapped);
    }
    return out;
  });
}
