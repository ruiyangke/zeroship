import { useCallback, useEffect, useState } from "react";
import { Select } from "@zeroship/ui";

import {
  addGroupMember,
  createGroup,
  listGroups,
  listProducts,
  listUsers,
  restrictProduct,
  unrestrictProduct,
} from "../api";
import { errorMessage } from "./rpc";

/**
 * Group administration: create a group and put people in it.
 *
 * Without this there is no way to reach the access-control model from the app
 * at all -- `bugs.restrict` needs a group id, and nothing could create one. The
 * whole security surface existed server-side and was unreachable.
 *
 * Admin-only, and it says so rather than rendering a form that will 403 on
 * submit. The first account to exist is the admin, the way Bugzilla's
 * installer creates one.
 */
export function GroupsAdmin() {
  const [groups, setGroups] = useState<Awaited<ReturnType<typeof listGroups>> | null>(null);
  const [denied, setDenied] = useState(false);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [memberGroup, setMemberGroup] = useState("");
  const [memberQuery, setMemberQuery] = useState("");
  const [matches, setMatches] = useState<Awaited<ReturnType<typeof listUsers>>>([]);

  const load = useCallback(async () => {
    try {
      setGroups(await listGroups({}));
      setDenied(false);
    } catch (err) {
      if (errorMessage(err).toLowerCase().includes("admin")) setDenied(true);
      else setError(errorMessage(err));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const create = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createGroup({ name: name.trim() });
      setName("");
      await load();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const search = async () => {
    try {
      setMatches(await listUsers({ text: memberQuery.trim(), limit: 10 }));
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const add = async (userId: string) => {
    if (!memberGroup) return;
    setBusy(true);
    setError(null);
    try {
      await addGroupMember({ groupId: memberGroup, userId });
      setMatches([]);
      setMemberQuery("");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  if (denied) {
    return (
      <section className="admin-section groups-admin">
        <h2>Groups</h2>
        <p className="state-hint small">
          Only an administrator can manage groups. The first account to exist becomes the
          administrator.
        </p>
      </section>
    );
  }

  return (
    <section className="admin-section groups-admin">
      <h2>Groups</h2>
      <p className="state-hint small">
        A group restricts what its members can see. Restrict a whole product, or one
        confidential bug inside an otherwise readable one.
      </p>

      <div className="field-row">
        <label>
          New group
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="security" />
        </label>
        <button type="button" className="btn primary small" disabled={busy || !name.trim()} onClick={() => void create()}>
          Create
        </button>
      </div>

      {groups === null ? (
        <p className="state-hint small">Loading groups...</p>
      ) : groups.length === 0 ? (
        <p className="state-hint small">No groups yet.</p>
      ) : (
        <ul className="group-list">
          {groups.map((group) => (
            <li key={group.id}>
              <strong>{group.name}</strong>
              {group.description ? <span className="dim"> {group.description}</span> : null}
            </li>
          ))}
        </ul>
      )}

      {groups && groups.length > 0 ? (
        <div className="field-row">
          <label>
            Add member to
            <Select
            value={memberGroup}
            onValueChange={(next) => setMemberGroup(next ?? "")}
            placeholder="Select a group"
            aria-label="Add member to"
            renderValue={(id) => (groups ?? []).find((x) => x.id === id)?.name ?? id}
          >
            {(groups ?? []).map((group) => (
              <Select.Item key={group.id} value={group.id}>
                {group.name}
              </Select.Item>
            ))}
          </Select>
          </label>
          <label>
            Find user
            <input
              value={memberQuery}
              onChange={(e) => setMemberQuery(e.target.value)}
              placeholder="name or email"
            />
          </label>
          <button type="button" className="btn ghost small" onClick={() => void search()}>
            Search
          </button>
        </div>
      ) : null}

      {matches.length > 0 ? (
        <ul className="user-matches">
          {matches.map((user) => (
            <li key={user.id}>
              {user.name} <span className="dim">@{user.handle}</span>
              <button
                type="button"
                className="btn ghost small"
                disabled={busy || !memberGroup}
                onClick={() => void add(user.id)}
              >
                Add
              </button>
            </li>
          ))}
        </ul>
      ) : null}

      {groups && groups.length > 0 ? <ProductRestrictions groups={groups} /> : null}

      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}

/**
 * Product-level restriction: hide an entire product behind a group.
 *
 * Distinct from restricting one bug. This is the coarser of the two controls
 * and the one that silently changes what a whole team can see, so the
 * confirmation names the consequence rather than saying "saved".
 */
function ProductRestrictions({ groups }: { groups: Awaited<ReturnType<typeof listGroups>> }) {
  const [products, setProducts] = useState<Awaited<ReturnType<typeof listProducts>>>([]);
  const [productId, setProductId] = useState("");
  const [groupId, setGroupId] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // Awaited rather than chained: the generated client types this as
    // `Row[] | Promise<Row[]>`, so `.then` does not exist on the union.
    void (async () => {
      try {
        setProducts(await listProducts({}));
      } catch (err) {
        setError(errorMessage(err));
      }
    })();
  }, []);

  const apply = async (action: "restrict" | "unrestrict") => {
    if (!productId || !groupId) return;
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      if (action === "restrict") await restrictProduct({ productId, groupId });
      else await unrestrictProduct({ productId, groupId });
      setNote(
        action === "restrict"
          ? "Restricted. Only members of that group can now see this product and its bugs."
          : "Restriction removed.",
      );
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="product-restrictions">
      <h3>Product visibility</h3>
      <div className="field-row">
        <label>
          Product
          <Select
            value={productId}
            onValueChange={(next) => setProductId(next ?? "")}
            placeholder="Select a product"
            aria-label="Product"
            renderValue={(id) => (products ?? []).find((x) => x.id === id)?.name ?? id}
          >
            {(products ?? []).map((p) => (
              <Select.Item key={p.id} value={p.id}>
                {p.name}
              </Select.Item>
            ))}
          </Select>
        </label>
        <label>
          Group
          <Select
            value={groupId}
            onValueChange={(next) => setGroupId(next ?? "")}
            placeholder="Select a group"
            aria-label="Group"
            renderValue={(id) => (groups ?? []).find((x) => x.id === id)?.name ?? id}
          >
            {(groups ?? []).map((group) => (
              <Select.Item key={group.id} value={group.id}>
                {group.name}
              </Select.Item>
            ))}
          </Select>
        </label>
        <button
          type="button"
          className="btn ghost small"
          disabled={busy || !productId || !groupId}
          onClick={() => void apply("restrict")}
        >
          Restrict
        </button>
        <button
          type="button"
          className="btn ghost small"
          disabled={busy || !productId || !groupId}
          onClick={() => void apply("unrestrict")}
        >
          Remove
        </button>
      </div>
      {note ? <p className="state-hint small">{note}</p> : null}
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}
