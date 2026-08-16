import { useState } from "react";
import { Button, Field, Input, Select } from "@zeroship/ui";

import {
  addGroupMember,
  removeGroupMember,
  createGroup,
  deleteGroup,
  restrictProduct,
  unrestrictProduct,
} from "../api";
import { invalidatedBy } from "../lib/query-keys";
import {
  useAppMutation,
  useGroupMembers,
  useGroups,
  useProducts,
  useUserSearch,
} from "../lib/queries";
import { errorMessage } from "./rpc";

/**
 * Group administration: create a group and put people in it.
 *
 * Without this there is no way to reach the access-control model from the app
 * at all -- `issues.restrict` needs a group id, and nothing could create one. The
 * whole security surface existed server-side and was unreachable.
 *
 * Admin-only, and it says so rather than rendering a form that will 403 on
 * submit. The first account to exist is the admin, the way Bugzilla's
 * installer creates one.
 */
export function GroupsAdmin() {
  const groupsQ = useGroups();
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);

  const [memberGroup, setMemberGroup] = useState("");
  const [memberQuery, setMemberQuery] = useState("");
  const [submittedMemberQuery, setSubmittedMemberQuery] = useState<string | null>(null);

  const matchesQ = useUserSearch(
    { text: submittedMemberQuery ?? "", limit: 10 },
    { enabled: submittedMemberQuery !== null },
  );
  const membersQ = useGroupMembers(memberGroup);
  const createGroupMutation = useAppMutation(
    (input: Parameters<typeof createGroup>[0]) => createGroup(input),
    () => invalidatedBy.groupsChanged(),
  );
  const deleteGroupMutation = useAppMutation(
    (input: Parameters<typeof deleteGroup>[0]) => deleteGroup(input),
    () => invalidatedBy.groupsChanged(),
  );
  const addMemberMutation = useAppMutation(
    (input: Parameters<typeof addGroupMember>[0]) => addGroupMember(input),
    ({ groupId }) => invalidatedBy.groupMembershipChanged(groupId),
  );
  const removeMemberMutation = useAppMutation(
    (input: Parameters<typeof removeGroupMember>[0]) => removeGroupMember(input),
    ({ groupId }) => invalidatedBy.groupMembershipChanged(groupId),
  );
  const groups = groupsQ.data;
  const matches = submittedMemberQuery === null ? [] : matchesQ.data ?? [];
  const members = membersQ.data ?? [];
  const busy =
    createGroupMutation.isPending ||
    deleteGroupMutation.isPending ||
    addMemberMutation.isPending ||
    removeMemberMutation.isPending;
  const groupsError = groupsQ.error ? errorMessage(groupsQ.error) : null;
  const denied = groupsError?.toLowerCase().includes("admin") ?? false;
  const queryError = denied ? null : groupsError ?? matchesQ.error ?? membersQ.error;
  const displayedError = error ?? (queryError ? errorMessage(queryError) : null);

  // Refusal is an ANSWER here, not a failure: the server declines while the
  // group still restricts issues or products and says how many, because
  // deleting it then would quietly widen who can read them. So the message
  // is surfaced rather than swallowed.
  const remove = async (groupId: string) => {
    setError(null);
    try {
      await deleteGroupMutation.mutateAsync({ id: groupId });
      if (memberGroup === groupId) {
        setMemberGroup("");
      }
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const create = async () => {
    if (!name.trim()) return;
    setError(null);
    try {
      await createGroupMutation.mutateAsync({ name: name.trim() });
      setName("");
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const search = () => {
    setError(null);
    const query = memberQuery.trim();
    if (query === submittedMemberQuery) {
      void matchesQ.refetch();
    } else {
      setSubmittedMemberQuery(query);
    }
  };

  const add = async (userId: string) => {
    if (!memberGroup) return;
    setError(null);
    try {
      await addMemberMutation.mutateAsync({ groupId: memberGroup, userId });
      setSubmittedMemberQuery(null);
      setMemberQuery("");
    } catch (err) {
      setError(errorMessage(err));
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

  const removeMember = async (userId: string) => {
    setError(null);
    try {
      await removeMemberMutation.mutateAsync({ groupId: memberGroup, userId });
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <section className="admin-section groups-admin">
      <h2>Groups</h2>
      <p className="state-hint small">
        A group restricts what its members can see. Restrict a whole product, or one
        confidential issue inside an otherwise readable one.
      </p>

      <div className="field-row">
        <Field>
          <Field.Label>New group</Field.Label>
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="security" />
        </Field>
        <Button variant="filled" size="sm" disabled={busy || !name.trim()} onClick={() => void create()}>
          Create
        </Button>
      </div>

      {groups === undefined ? (
        <p className="state-hint small">Loading groups...</p>
      ) : groups.length === 0 ? (
        <p className="state-hint small">No groups yet.</p>
      ) : (
        <>
          {/* Counted and BOUNDED. This was an unbounded bulleted list, so a
              tracker with thirty groups pushed products, components, versions
              and flag types off the bottom of a page called "Products
              administration" -- the groups section is not even its subject.
              The count is stated because a scroll container hides how much is
              in it. */}
          <p className="state-hint small">
            {groups.length} {groups.length === 1 ? "group" : "groups"}
          </p>
          <ul className="group-list">
            {groups.map((group) => (
              <li key={group.id}>
                <strong>{group.name}</strong>
                {group.description ? <span className="dim"> {group.description}</span> : null}
                {/* Named per group. A list of identical "Delete" buttons is
                    ambiguous to a screen reader, and this one destroys an
                    access-control object. */}
                <Button
                  variant="gray"
                  size="sm"
                  intent="destructive"
                  disabled={busy}
                  aria-label={`Delete group ${group.name}`}
                  onClick={() => void remove(group.id)}
                >
                  Delete
                </Button>
              </li>
            ))}
          </ul>
        </>
      )}

      {groups && groups.length > 0 ? (
        <div className="field-row">
          <Field>
            <Field.Label>Add member to</Field.Label>
            <Select
              value={memberGroup}
              onValueChange={(next) => {
                setMemberGroup(next ?? "");
              }}
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
          </Field>
          <Field>
            <Field.Label>Find user</Field.Label>
            <Input
              value={memberQuery}
              onChange={(e) => setMemberQuery(e.target.value)}
              placeholder="name or email"
            />
          </Field>
          <Button variant="gray" size="sm" onClick={search}>
            Search
          </Button>
        </div>
      ) : null}

      {/* Who is already in the chosen group, and a way out.
          groups.addMember and groups.removeMember both existed with nothing
          between them to list members, so access could be granted here and
          never seen or revoked -- the half of an access-control surface that
          actually matters. */}
      {memberGroup ? (
        <div>
          <h3>Members</h3>
          {members.length === 0 ? (
            <p className="state-hint small">Nobody is in this group yet.</p>
          ) : (
            <ul className="member-list">
              {members.map((member) => (
                <li key={member.id}>
                  <span>{member.name ?? member.handle}</span>
                  <Button
                    variant="gray"
                    size="sm"
                    intent="destructive"
                    disabled={busy}
                    onClick={() => void removeMember(member.id)}
                  >
                    Remove
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : null}

      {matches.length > 0 ? (
        <ul>
          {matches.map((user) => (
            <li key={user.id}>
              {user.name} <span className="dim">@{user.handle}</span>
              <Button variant="gray" size="sm"
                disabled={busy || !memberGroup}
                onClick={() => void add(user.id)}
              >
                Add
              </Button>
            </li>
          ))}
        </ul>
      ) : null}

      {groups && groups.length > 0 ? <ProductRestrictions groups={groups} /> : null}

      {displayedError ? <p className="field-error">{displayedError}</p> : null}
    </section>
  );
}

/**
 * Product-level restriction: hide an entire product behind a group.
 *
 * Distinct from restricting one issue. This is the coarser of the two controls
 * and the one that silently changes what a whole team can see, so the
 * confirmation names the consequence rather than saying "saved".
 */
function ProductRestrictions({
  groups,
}: {
  groups: NonNullable<ReturnType<typeof useGroups>["data"]>;
}) {
  const productsQ = useProducts();
  const [productId, setProductId] = useState("");
  const [groupId, setGroupId] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // The query owns the generated client's `Row[] | Promise<Row[]>` return
  // shape, so this panel consumes one resolved, cached products list.
  const products = productsQ.data ?? [];
  const changeRestriction = useAppMutation(
    ({ action, productId, groupId }: {
      action: "restrict" | "unrestrict";
      productId: string;
      groupId: string;
    }) =>
      action === "restrict"
        ? restrictProduct({ productId, groupId })
        : unrestrictProduct({ productId, groupId }),
    () => invalidatedBy.productStructureChanged(),
  );
  const busy = changeRestriction.isPending;
  const displayedError = error ?? (productsQ.error ? errorMessage(productsQ.error) : null);

  const apply = async (action: "restrict" | "unrestrict") => {
    if (!productId || !groupId) return;
    setError(null);
    setNote(null);
    try {
      await changeRestriction.mutateAsync({ action, productId, groupId });
      setNote(
        action === "restrict"
          ? "Restricted. Only members of that group can now see this product and its issues."
          : "Restriction removed.",
      );
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <div>
      <h3>Product visibility</h3>
      <div className="field-row">
        <Field>
          <Field.Label>Product</Field.Label>
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
        </Field>
        <Field>
          <Field.Label>Group</Field.Label>
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
        </Field>
        <Button variant="gray" size="sm"
          disabled={busy || !productId || !groupId}
          onClick={() => void apply("restrict")}
        >
          Restrict
        </Button>
        <Button variant="gray" size="sm"
          disabled={busy || !productId || !groupId}
          onClick={() => void apply("unrestrict")}
        >
          Remove
        </Button>
      </div>
      {note ? <p className="state-hint small">{note}</p> : null}
      {displayedError ? <p className="field-error">{displayedError}</p> : null}
    </div>
  );
}
