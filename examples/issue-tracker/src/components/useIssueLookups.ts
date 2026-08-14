import { useMemo } from "react";

import { useResolvedProducts, useResolvedUsers } from "../lib/queries";

type Row = { productId?: string | null; assigneeId?: string | null; reporterId?: string | null };

/**
 * The id-to-label maps an issue table needs, for the rows it is actually showing.
 *
 * An issue row carries `productId` and `assigneeId`, and a table that renders
 * them raw prints `prod_034607nk...` and `user_0345pl8p...` in the columns a
 * reader scans.
 *
 * Resolves the ids ON SCREEN rather than fetching the whole world. The first
 * version built its maps from all of `products.list` and `users.list`, which
 * was wrong in two ways. `users.list` caps at 100 rows, so a tracker with more
 * than a hundred people already rendered ids for anyone past the cap -- a live
 * bug, not a scale worry. And `products.list` returning everything is what
 * blocked paginating it: a limit there would have turned rows past the limit
 * back into ids, silently, which is the defect these maps exist to prevent.
 *
 * Both queries are allowed to fail. `users.resolve` needs authentication while
 * issue search does not, so an anonymous reader legitimately gets nothing back
 * and the table falls back to the id, as it always could.
 */
// `rows` is REQUIRED and has no default. A default of `[]` typechecks at every
// existing call site and resolves nothing, so every table would render raw ids
// again with the compiler silent -- the same shape as the optional
// `productsById` prop this hook was written to replace. Defaulting it once here
// would have undone the fix it implements.
export function useIssueLookups(rows: readonly Row[]): {
  productsById: Record<string, string>;
  productKeysById: Record<string, string>;
  usersById: Record<string, string>;
} {
  // Keyed on the sorted id set, not the rows array. Re-fetching whenever the
  // caller happens to build a new array would refetch on every render; the
  // lookups only need to change when the ids do.
  const productIds = useMemo(
    () => [...new Set(rows.map((row) => row.productId).filter(Boolean) as string[])].sort(),
    [rows],
  );
  const userIds = useMemo(
    () =>
      [
        ...new Set(
          rows.flatMap((row) => [row.assigneeId, row.reporterId]).filter(Boolean) as string[],
        ),
      ].sort(),
    [rows],
  );

  // The joined-string dependency keys are gone with the hook that needed
  // them: the cache keys on the SORTED id array itself, so identity is the
  // question rather than a hand-made stand-in for it, and two tables showing
  // the same rows now share one answer instead of asking separately.
  const productsQ = useResolvedProducts(productIds);
  const usersQ = useResolvedUsers(userIds);

  const productsById = useMemo(() => {
    const map: Record<string, string> = {};
    if (productsQ.data) {
      for (const product of productsQ.data) map[product.id] = product.name;
    }
    return map;
  }, [productsQ.data]);

  const productKeysById = useMemo(() => {
    const map: Record<string, string> = {};
    if (productsQ.data) {
      for (const product of productsQ.data) map[product.id] = product.key;
    }
    return map;
  }, [productsQ.data]);

  const usersById = useMemo(() => {
    const map: Record<string, string> = {};
    if (usersQ.data) {
      for (const user of usersQ.data) map[user.id] = user.handle;
    }
    return map;
  }, [usersQ.data]);

  return { productsById, productKeysById, usersById };
}
