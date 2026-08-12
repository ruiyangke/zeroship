import { useMemo } from "react";

import { resolveProducts, resolveUsers } from "../api";
import { useAsync } from "./rpc";

type Row = { productId?: string | null; assigneeId?: string | null; reporterId?: string | null };

/**
 * The id-to-label maps a bug table needs, for the rows it is actually showing.
 *
 * A bug row carries `productId` and `assigneeId`, and a table that renders
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
 * bug search does not, so an anonymous reader legitimately gets nothing back
 * and the table falls back to the id, as it always could.
 */
// `rows` is REQUIRED and has no default. A default of `[]` typechecks at every
// existing call site and resolves nothing, so every table would render raw ids
// again with the compiler silent -- the same shape as the optional
// `productsById` prop this hook was written to replace. Defaulting it once here
// would have undone the fix it implements.
export function useBugLookups(rows: readonly Row[]): {
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

  const productKey = productIds.join(",");
  const userKey = userIds.join(",");

  const productsQ = useAsync(
    () => resolveProducts({ ids: productIds }),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the joined key IS
    // the identity of productIds; depending on the array refetches per render.
    [productKey],
  );
  const usersQ = useAsync(
    () => resolveUsers({ ids: userIds }),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- see above.
    [userKey],
  );

  const productsById = useMemo(() => {
    const map: Record<string, string> = {};
    if (productsQ.state.status === "ready") {
      for (const product of productsQ.state.data) map[product.id] = product.name;
    }
    return map;
  }, [productsQ.state]);

  const productKeysById = useMemo(() => {
    const map: Record<string, string> = {};
    if (productsQ.state.status === "ready") {
      for (const product of productsQ.state.data) map[product.id] = product.key;
    }
    return map;
  }, [productsQ.state]);

  const usersById = useMemo(() => {
    const map: Record<string, string> = {};
    if (usersQ.state.status === "ready") {
      for (const user of usersQ.state.data) map[user.id] = user.handle;
    }
    return map;
  }, [usersQ.state]);

  return { productsById, productKeysById, usersById };
}
