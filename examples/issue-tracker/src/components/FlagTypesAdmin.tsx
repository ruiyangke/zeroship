import { useState } from "react";

import { Input, Select } from "@zeroship/ui";

import { createFlagType } from "../api";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useFlagTypes, useProducts } from "../lib/queries";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { errorMessage } from "./rpc";
import { FieldError, Hint, InlineForm, Muted, SectionHeading } from "./AppPrimitives";

/**
 * Flag type administration.
 *
 * The same hole `GroupsAdmin` closed, in the flag feature: `flags.set`,
 * `flags.clear`, `flags.list` and `flags.listRequests` all need a
 * `flagTypeId`, the issue page renders a Flags panel, and SPEC.md listed flags
 * as delivered -- but nothing in the app, the migration or any seed could put
 * a row in `flagTypes`. Every product reported "defines no issue-level flag
 * types" and always would have.
 *
 * Admin-only, matching Bugzilla, where flag types are defined by an
 * administrator rather than invented by a reporter while filing.
 */
export function FlagTypesAdmin() {
  const typesQ = useFlagTypes();
  const productsQ = useProducts();
  const [denied, setDenied] = useState(false);
  const [name, setName] = useState("");
  const [targetType, setTargetType] = useState<"issue" | "attachment">("issue");
  const [productId, setProductId] = useState("");
  const [error, setError] = useState<string | null>(null);

  const createType = useAppMutation(
    (input: Parameters<typeof createFlagType>[0]) => createFlagType(input),
    () => invalidatedBy.flagTypeCreated(),
  );
  const types = typesQ.data;
  const products = productsQ.data ?? [];
  const busy = createType.isPending;
  // Listing is not admin-gated, so a query failure here is a real error rather
  // than the not-an-admin case. `denied` is set by the CREATE below.
  const queryError = typesQ.error ?? productsQ.error;
  const displayedError = error ?? (queryError ? errorMessage(queryError) : null);

  const create = async () => {
    if (!name.trim()) return;
    setError(null);
    try {
      await createType.mutateAsync({
        name: name.trim(),
        targetType,
        productId: productId || null,
      });
      setName("");
      setDenied(false);
    } catch (err) {
      const message = errorMessage(err);
      // Says so rather than leaving a form that will keep failing. The first
      // account to exist is the admin.
      if (/admin/i.test(message)) setDenied(true);
      setError(message);
    }
  };

  return (
    <section className="flag-types-admin">
      <SectionHeading>Flag types</SectionHeading>
      <Hint>
        A flag is a named request or sign-off on an issue or an attachment. Until a type exists
        here, the Flags panel on every issue stays empty.
      </Hint>
      {denied ? (
        <Hint>Only an administrator can define flag types.</Hint>
      ) : null}
      <InlineForm>
        <Field.Root>
          <Field.Label>New flag type</Field.Label>
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="review" />
        </Field.Root>
        <Field.Root>
          <Field.Label>Product</Field.Label>
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
        </Field.Root>
        <Field.Root>
          <Field.Label>Applies to</Field.Label>
          <Select
            value={targetType}
            onValueChange={(next) => setTargetType((next as "issue" | "attachment") ?? "issue")}
            aria-label="Applies to"
          >
            <Select.Item value="issue">issue</Select.Item>
            <Select.Item value="attachment">attachment</Select.Item>
          </Select>
        </Field.Root>
        <Button variant="filled"
          disabled={busy || !name.trim()}
          onClick={() => void create()}
        >
          Create
        </Button>
      </InlineForm>
      {displayedError ? <FieldError>{displayedError}</FieldError> : null}
      {types === undefined ? (
        <Hint>Loading flag types...</Hint>
      ) : types.length === 0 ? (
        <Hint>No flag types defined yet.</Hint>
      ) : (
        <ul className="flag-type-list">
          {types.map((type) => (
            <li key={type.id}>
              <b>{type.name}</b> <Muted>({type.targetType})</Muted>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
