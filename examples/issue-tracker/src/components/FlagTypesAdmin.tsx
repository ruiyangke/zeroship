import { useCallback, useEffect, useState } from "react";

import { Field, Input, Select } from "@zeroship/ui";

import { createFlagType, listFlagTypes, listProducts } from "../api";
import { errorMessage } from "./rpc";

/**
 * Flag type administration.
 *
 * The same hole `GroupsAdmin` closed, in the flag feature: `flags.set`,
 * `flags.clear`, `flags.list` and `flags.listRequests` all need a
 * `flagTypeId`, the bug page renders a Flags panel, and SPEC.md listed flags
 * as delivered -- but nothing in the app, the migration or any seed could put
 * a row in `flagTypes`. Every product reported "defines no bug-level flag
 * types" and always would have.
 *
 * Admin-only, matching Bugzilla, where flag types are defined by an
 * administrator rather than invented by a reporter while filing.
 */
export function FlagTypesAdmin() {
  const [types, setTypes] = useState<Awaited<ReturnType<typeof listFlagTypes>> | null>(null);
  const [denied, setDenied] = useState(false);
  const [name, setName] = useState("");
  const [targetType, setTargetType] = useState<"bug" | "attachment">("bug");
  const [productId, setProductId] = useState("");
  const [products, setProducts] = useState<Awaited<ReturnType<typeof listProducts>>>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setTypes(await listFlagTypes({}));
      setProducts(await listProducts({}));
      setDenied(false);
    } catch (err) {
      // Listing is not admin-gated, so a failure here is a real error rather
      // than the not-an-admin case. `denied` is set by the CREATE below.
      setError(errorMessage(err));
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
      await createFlagType({ name: name.trim(), targetType, productId: productId || null });
      setName("");
      await load();
    } catch (err) {
      const message = errorMessage(err);
      // Says so rather than leaving a form that will keep failing. The first
      // account to exist is the admin.
      if (/admin/i.test(message)) setDenied(true);
      setError(message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="flag-types-admin">
      <h2>Flag types</h2>
      <p className="state-hint small">
        A flag is a named request or sign-off on a bug or an attachment. Until a type exists
        here, the Flags panel on every bug stays empty.
      </p>
      {denied ? (
        <p className="state-hint small">Only an administrator can define flag types.</p>
      ) : null}
      <div className="inline-form">
        <Field>
          <Field.Label>New flag type</Field.Label>
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="review" />
        </Field>
        <label>
          Product
          <Select value={productId} onValueChange={(v) => setProductId(v ?? "")} placeholder="All products" aria-label="Product" renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}>
            {/* A type with no product applies to EVERY product, which is
                Bugzilla behaviour and worth choosing rather than defaulting
                into: the first version of this panel always created global
                types, so one product defining "review" silently offered it on
                all of them. */}
            {products.map((p) => (
              <Select.Item key={p.id} value={p.id}>
                {p.name}
              </Select.Item>
            ))}
          </Select>
        </label>
        <label>
          Applies to
          <Select
            value={targetType}
            onValueChange={(next) => setTargetType((next as "bug" | "attachment") ?? "bug")}
            aria-label="Applies to"
          >
            <Select.Item value="bug">bug</Select.Item>
            <Select.Item value="attachment">attachment</Select.Item>
          </Select>
        </label>
        <button
          type="button"
          className="btn primary small"
          disabled={busy || !name.trim()}
          onClick={() => void create()}
        >
          Create
        </button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      {types === null ? (
        <p className="state-hint small">Loading flag types...</p>
      ) : types.length === 0 ? (
        <p className="state-hint small">No flag types defined yet.</p>
      ) : (
        <ul className="flag-type-list">
          {types.map((type) => (
            <li key={type.id}>
              <b>{type.name}</b> <span className="dim">({type.targetType})</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
