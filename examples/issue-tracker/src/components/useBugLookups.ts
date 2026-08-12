import { useMemo } from "react";

import { listProducts, listUsers } from "../api";
import { useAsync } from "./rpc";

/**
 * The id-to-label maps every bug table needs.
 *
 * A bug row carries `productId` and `assigneeId`, and a table that renders
 * them raw prints `prod_034607nk...` and `user_0345pl8p...` in the columns a
 * reader scans. The bug list built these maps inline; the dashboard and both
 * advanced-search tables did not, so three of the four tables in the app
 * showed ids where the fourth showed names.
 *
 * Both queries are allowed to fail. `users.list` requires authentication while
 * bug search does not, so an anonymous reader legitimately gets nothing back
 * here -- and a table of bugs with unresolved names is still worth rendering.
 * `useAsync` leaves the state non-ready in that case and the maps stay empty,
 * which is exactly the id fallback the table already has.
 */
export function useBugLookups(): {
  productsById: Record<string, string>;
  usersById: Record<string, string>;
} {
  const productsQ = useAsync(() => listProducts({}), []);
  const usersQ = useAsync(() => listUsers({}), []);

  const productsById = useMemo(() => {
    const map: Record<string, string> = {};
    if (productsQ.state.status === "ready") {
      for (const product of productsQ.state.data) map[product.id] = product.name;
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

  return { productsById, usersById };
}
