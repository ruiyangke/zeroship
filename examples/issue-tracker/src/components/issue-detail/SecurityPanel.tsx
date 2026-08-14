import { useCallback, useEffect, useState } from "react";
import { Button, Field, Select } from "@zeroship/ui";

import { listGroups, restrictIssue, unrestrictIssue } from "../../api";
import { errorMessage } from "../rpc";

/**
 * Issue-level security groups -- Bugzilla's bug_group_map, the mechanism behind
 * a confidential issue inside an otherwise readable product.
 *
 * The server has had `issues.restrict` / `issues.unrestrict` and the whole group
 * model for a while and NO interface reached any of it, so the app's most
 * consequential feature could only be exercised over raw RPC. That is the same
 * gap the flags, CC and voting panels each had.
 *
 * `groups.list` is admin-only, so a non-admin sees an explanation instead of an
 * empty picker -- an empty control would read as "there are no groups" when
 * the truth is "you cannot see them".
 */
export function SecurityPanel({ issueId, onChanged }: { issueId: string; onChanged: () => void }) {
  const [groups, setGroups] = useState<Awaited<ReturnType<typeof listGroups>> | null>(null);
  const [denied, setDenied] = useState(false);
  const [selected, setSelected] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setGroups(await listGroups({}));
      setDenied(false);
    } catch (err) {
      // A 403 here is the ordinary case for a non-admin, not a failure worth
      // shouting about; anything else is.
      if (errorMessage(err).toLowerCase().includes("admin")) setDenied(true);
      else setError(errorMessage(err));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const apply = async (action: "restrict" | "unrestrict") => {
    if (!selected) return;
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      if (action === "restrict") await restrictIssue({ issueId, groupId: selected });
      else await unrestrictIssue({ issueId, groupId: selected });
      setNote(
        action === "restrict"
          ? "Restricted. Only members of that group can now see this issue."
          : "Restriction removed.",
      );
      // Deliberately NOT calling onChanged().
      //
      // The parent's reload puts the page back into its loading state, which
      // unmounts the whole detail tree -- so this panel remounts, `note` and
      // `selected` reset, and the confirmation the user needs to see is
      // destroyed by the act of succeeding. Nothing on this page reflects a
      // group restriction anyway: the issue row is unchanged, and the viewer
      // making the change can still see it either way.
      void onChanged;
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="security-panel">
      <h3>Security</h3>
      {denied ? (
        <p className="state-hint small">
          Only an administrator can see and change which groups an issue is restricted to.
        </p>
      ) : groups === null ? (
        <p className="state-hint small">Loading groups...</p>
      ) : groups.length === 0 ? (
        <p className="state-hint small">
          No groups exist yet. Create one under Products to restrict this issue.
        </p>
      ) : (
        <>
          <Field>
            <Field.Label>Group</Field.Label>
            {/* The design system Select. The native one rendered its options
                with the surrounding JSX whitespace, so selecting by label
                matched nothing and silently left the control unset -- a spec
                had to read the value off the option element to work around
                it. */}
            <Select
              value={selected}
              onValueChange={(next) => setSelected(next ?? "")}
              placeholder="Select a group"
              aria-label="Group"
              renderValue={(id) => groups.find((g) => g.id === id)?.name ?? id}
            >
              {groups.map((group) => (
                <Select.Item key={group.id} value={group.id}>
                  {group.name}
                </Select.Item>
              ))}
            </Select>
          </Field>
          <Button variant="gray" size="small"
            disabled={busy || !selected}
            onClick={() => void apply("restrict")}
          >
            Restrict
          </Button>
          <Button variant="gray" size="small"
            disabled={busy || !selected}
            onClick={() => void apply("unrestrict")}
          >
            Remove
          </Button>
          {note ? <p className="state-hint small">{note}</p> : null}
          {error ? <p className="field-error">{error}</p> : null}
        </>
      )}
    </section>
  );
}
