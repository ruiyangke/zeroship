import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Input } from "@zeroship/ui";
import { Button } from "../../ui/Button";
import { addDependency, removeDependency } from "../../api";
import { StatusBadge } from "../Badges";
import { FieldError, Hint, InlineForm, Muted } from "../AppPrimitives";
import { AsyncSection } from "../StateViews";
import { errorMessage } from "../rpc";
import { useAppMutation, useDependencyGraph, useDuplicates } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
import { Absent, Pending } from "./Absent";
import { RailDisclosure } from "./RailDisclosure";
import { RailList } from "./RailList";

function IssueLink({ id, summary, status }: { id: string; summary: string; status: string }) {
  return (
    <Link to={`/issues/${id}`} className="inline-flex items-center gap-2">
      <StatusBadge status={status} />
      <span>{summary}</span>
    </Link>
  );
}

export function DependenciesPanel({ issueId }: { issueId: string }) {
  const graphQ = useDependencyGraph(issueId);
  const [newDep, setNewDep] = useState("");
  const [error, setError] = useState<string | null>(null);

  // Both writes move the relation graph, so both name the same blast radius:
  // this panel's query, its sibling panels, and the issue itself. Nothing has
  // to be handed a callback to hear about it.
  const addDep = useAppMutation(
    (dependsOnId: string) => addDependency({ issueId, dependsOnId }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const removeDep = useAppMutation(
    (dependsOnId: string) => removeDependency({ issueId, dependsOnId }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const busy = addDep.isPending || removeDep.isPending;

  const add = async () => {
    if (!newDep.trim()) return;
    setError(null);
    try {
      await addDep.mutateAsync(newDep.trim());
      // Clearing the field is the only thing left for the caller to do; the
      // refresh is the mutation's own business.
      setNewDep("");
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const remove = async (dependsOnId: string) => {
    setError(null);
    try {
      await removeDep.mutateAsync(dependsOnId);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const counts = graphQ.data
    ? {
        dependsOn: graphQ.data.edges.filter((e) => e.issueId === issueId).length,
        blocks: graphQ.data.edges.filter((e) => e.dependsOnId === issueId).length,
      }
    : null;
  const summary = !counts ? (
    <Pending width="7rem" />
  ) : counts.dependsOn === 0 && counts.blocks === 0 ? (
    <Absent />
  ) : (
    <>
      {counts.dependsOn} depends on, {counts.blocks} blocks
    </>
  );

  return (
    <RailDisclosure label="Dependencies" summary={summary}>
      <section className="relations-panel">
        <AsyncSection
          query={graphQ}
          loadingLabel="Loading dependencies..."
          isEmpty={(data) => data.nodes.length <= 1}
          emptyTitle="No dependencies."
          emptyTone="inline"
          emptyHint="This issue doesn't block, or depend on, any other issue."
        >
          {(graph) => {
            const byId = new Map(graph.nodes.map((n) => [n.id, n]));
            const dependsOn = graph.edges.filter((e) => e.issueId === issueId).map((e) => e.dependsOnId);
            const blocks = graph.edges.filter((e) => e.dependsOnId === issueId).map((e) => e.issueId);
            return (
              <div>
                <div>
                  <h4 className="mb-2">Depends on</h4>
                  {dependsOn.length === 0 ? (
                    <Hint>Nothing.</Hint>
                  ) : (
                    <RailList>
                      {dependsOn.map((id) => {
                        const node = byId.get(id);
                        return (
                          <li key={id}>
                            {node ? <IssueLink id={id} summary={node.summary} status={node.status} /> : id}
                            <Button variant="gray" disabled={busy} onClick={() => void remove(id)}>
                              Remove
                            </Button>
                          </li>
                        );
                      })}
                    </RailList>
                  )}
                </div>
                <div>
                  <h4 className="mb-2">Blocks</h4>
                  {blocks.length === 0 ? (
                    <Hint>Nothing.</Hint>
                  ) : (
                    <RailList>
                      {blocks.map((id) => {
                        const node = byId.get(id);
                        return (
                          <li key={id}>{node ? <IssueLink id={id} summary={node.summary} status={node.status} /> : id}</li>
                        );
                      })}
                    </RailList>
                  )}
                </div>
              </div>
            );
          }}
        </AsyncSection>
        {!graphQ.isError ? (
          <InlineForm>
            <Input aria-label="Issue this depends on" placeholder="PARSER-12" value={newDep} onChange={(e) => setNewDep(e.target.value)} />
            <Button variant="gray" disabled={busy || !newDep.trim()} onClick={() => void add()}>
              Add dependency
            </Button>
          </InlineForm>
        ) : null}
        {error ? <FieldError>{error}</FieldError> : null}
      </section>
    </RailDisclosure>
  );
}

export function DuplicatesPanel({
  issueId,
  duplicateOfId,
  labels = {},
}: {
  issueId: string;
  duplicateOfId: string | null;
  /** Ids to the names they stand for, shared with the history and timeline. */
  labels?: Record<string, string>;
}) {
  const duplicatesQ = useDuplicates(issueId);

  const cluster = useMemo(
    () => (duplicatesQ.data ? duplicatesQ.data.filter((b) => b.id !== issueId) : null),
    [duplicatesQ.data, issueId],
  );

  // `duplicateOfId` comes from the issue we already have, so it answers before
  // the cluster query does. Only the LAST branch is a real emptiness claim --
  // testing `cluster.length > 0` alone would print "no duplicates" for the
  // moment the list is still in flight. `cluster` is null exactly while there
  // is no answer to give: `isPending` on a first load, and still nothing to
  // report if the query failed. Both of those are the Skeleton, never `--`.
  const summary = duplicateOfId ? (
    <>duplicate of another issue</>
  ) : !cluster ? (
    <Pending width="8rem" />
  ) : cluster.length > 0 ? (
    <>{cluster.length} marked duplicate</>
  ) : (
    <Absent />
  );

  return (
    <RailDisclosure label="Duplicates" summary={summary}>
      <section className="relations-panel">
        {duplicateOfId ? (
          <Hint>
          This issue is marked as a duplicate of{" "}
          <Link to={`/issues/${duplicateOfId}`}>{labels[duplicateOfId] ?? duplicateOfId}</Link>.
          </Hint>
        ) : null}
        <AsyncSection
          query={duplicatesQ}
          loadingLabel="Loading duplicates..."
          isEmpty={() => (cluster?.length ?? 0) === 0}
          emptyTitle="No known duplicates."
          emptyTone="inline"
        >
          {() => (
            <RailList>
              {cluster?.map((b) => (
                <li key={b.id}>
                  <IssueLink id={b.id} summary={b.summary} status={b.status} />
                  {b.duplicateOfId === issueId ? <Muted> (duplicate of this issue)</Muted> : null}
                </li>
              ))}
            </RailList>
          )}
        </AsyncSection>
      </section>
    </RailDisclosure>
  );
}
