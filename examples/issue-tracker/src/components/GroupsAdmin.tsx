import { useCallback, useEffect, useState } from "react";

import { addGroupMember, createGroup, listGroups, listUsers } from "../api";
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
            <select value={memberGroup} onChange={(e) => setMemberGroup(e.target.value)}>
              <option value="">Select a group</option>
              {groups.map((group) => (
                <option key={group.id} value={group.id}>
                  {group.name}
                </option>
              ))}
            </select>
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

      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}
