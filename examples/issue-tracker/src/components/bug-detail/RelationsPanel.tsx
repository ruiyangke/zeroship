import { useMemo, useState } from "react";
import { Button } from "@zeroship/ui";
import { addDependency, dependencyGraph, listDuplicates, removeDependency } from "../../api";
import { StatusBadge } from "../Badges";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";

function BugLink({ id, summary, status }: { id: string; summary: string; status: string }) {
  return (
    <a href={`#/bugs/${id}`} className="relation-link">
      <StatusBadge status={status} />
      <span>{summary}</span>
    </a>
  );
}

export function DependenciesPanel({ bugId }: { bugId: string }) {
  const { state, reload } = useAsync(() => dependencyGraph({ bugId }), [bugId]);
  const [newDep, setNewDep] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const add = async () => {
    if (!newDep.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await addDependency({ bugId, dependsOnId: newDep.trim() });
      setNewDep("");
      reload();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (dependsOnId: string) => {
    setBusy(true);
    setError(null);
    try {
      await removeDependency({ bugId, dependsOnId });
      reload();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="relations-panel">
      <h3>Dependencies</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading dependencies..."
        isEmpty={(data) => data.nodes.length <= 1}
        emptyTitle="No dependencies."
        emptyHint="This bug doesn't block, or depend on, any other bug."
      >
        {(graph) => {
          const byId = new Map(graph.nodes.map((n) => [n.id, n]));
          const dependsOn = graph.edges.filter((e) => e.bugId === bugId).map((e) => e.dependsOnId);
          const blocks = graph.edges.filter((e) => e.dependsOnId === bugId).map((e) => e.bugId);
          return (
            <div className="relation-columns">
              <div>
                <h4>Depends on</h4>
                {dependsOn.length === 0 ? (
                  <p className="state-hint small">Nothing.</p>
                ) : (
                  <ul>
                    {dependsOn.map((id) => {
                      const node = byId.get(id);
                      return (
                        <li key={id}>
                          {node ? <BugLink id={id} summary={node.summary} status={node.status} /> : id}
                          <Button variant="gray" size="small" disabled={busy} onClick={() => void remove(id)}>
                            Remove
                          </Button>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </div>
              <div>
                <h4>Blocks</h4>
                {blocks.length === 0 ? (
                  <p className="state-hint small">Nothing.</p>
                ) : (
                  <ul>
                    {blocks.map((id) => {
                      const node = byId.get(id);
                      return (
                        <li key={id}>{node ? <BugLink id={id} summary={node.summary} status={node.status} /> : id}</li>
                      );
                    })}
                  </ul>
                )}
              </div>
            </div>
          );
        }}
      </AsyncSection>
      {state.status !== "error" ? (
        <div className="inline-form">
          <input placeholder="bug_... this depends on" value={newDep} onChange={(e) => setNewDep(e.target.value)} />
          <Button variant="gray" size="small" disabled={busy || !newDep.trim()} onClick={() => void add()}>
            Add dependency
          </Button>
        </div>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}

export function DuplicatesPanel({ bugId, duplicateOfId }: { bugId: string; duplicateOfId: string | null }) {
  const { state, reload } = useAsync(() => listDuplicates({ bugId }), [bugId]);

  const cluster = useMemo(() => {
    if (state.status !== "ready") return null;
    return state.data.filter((b) => b.id !== bugId);
  }, [state, bugId]);

  return (
    <section className="relations-panel">
      <h3>Duplicates</h3>
      {duplicateOfId ? (
        <p className="state-hint small">
          This bug is marked as a duplicate of{" "}
          <a href={`#/bugs/${duplicateOfId}`}>{duplicateOfId}</a>.
        </p>
      ) : null}
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading duplicates..."
        isEmpty={() => (cluster?.length ?? 0) === 0}
        emptyTitle="No known duplicates."
      >
        {() => (
          <ul>
            {cluster?.map((b) => (
              <li key={b.id}>
                <BugLink id={b.id} summary={b.summary} status={b.status} />
                {b.duplicateOfId === bugId ? <span className="dim"> (duplicate of this bug)</span> : null}
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
    </section>
  );
}
