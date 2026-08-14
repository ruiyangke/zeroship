import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Button, Checkbox, Field, Input, Select } from "@zeroship/ui";
import { createIssue } from "../api";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useProduct, useProducts } from "../lib/queries";
import { AsyncSection } from "../components/StateViews";
import { errorMessage } from "../components/rpc";
import { isVisitor, useSession } from "../components/session";
import {
  ISSUE_KINDS,
  ISSUE_PRIORITIES,
  ISSUE_SEVERITIES,
  type IssueKind,
  type IssuePriority,
  type IssueSeverity,
} from "../lib/quicksearch";

export function NewIssuePage() {
  const navigate = useNavigate();
  // ONE identity for the whole app, read from the session rather than asked
  // again here. This page calling `currentUser` itself was one of the four
  // `users.me` requests a single visit used to make.
  const session = useSession();
  // The active list, keyed by that input: the admin page asks for the same
  // procedure WITH inactive products and gets its own entry, which is why the
  // argument is part of the key.
  const productsQ = useProducts();

  const [productId, setProductId] = useState("");
  // The chosen product's structure is a keyed read of `products.get`, not a
  // copy of it in component state. The manual fetch-into-useState this
  // replaced could not tell "not chosen yet" from "still loading", and held a
  // second copy of a fact the cache already had.
  const productQ = useProduct(productId || null);
  const productDetail = productQ.data ?? null;
  const [componentId, setComponentId] = useState("");
  const [versionId, setVersionId] = useState("");
  const [milestoneId, setMilestoneId] = useState("");
  const [summary, setSummary] = useState("");
  const [description, setDescription] = useState("");
  const [kind, setKind] = useState<IssueKind>("defect");
  const [severity, setSeverity] = useState<IssueSeverity>("normal");
  const [priority, setPriority] = useState<IssuePriority>("P3");
  const [whiteboard, setWhiteboard] = useState("");
  const [opSys, setOpSys] = useState("Unspecified");
  const [platform, setPlatform] = useState("Unspecified");
  const [url, setUrl] = useState("");
  const [confirmed, setConfirmed] = useState(false);

  // Filing moves the list and the report totals; nothing else here writes.
  const create = useAppMutation(
    (input: Parameters<typeof createIssue>[0]) => createIssue(input),
    () => invalidatedBy.issueCreated(),
  );

  // Choosing a product only sets the id now -- the detail follows from the key.
  // The dependent fields are still cleared here, because a component or version
  // chosen under the previous product is not a stale copy of anything, it is
  // simply wrong once the product changes.
  const pickProduct = (id: string) => {
    setProductId(id);
    setComponentId("");
    setVersionId("");
    setMilestoneId("");
  };

  const submit = () => {
    if (!productId || !componentId || !summary.trim() || !description.trim()) return;
    create.mutate(
      {
        productId,
        componentId,
        summary: summary.trim(),
        description: description.trim(),
        versionId: versionId || undefined,
        milestoneId: milestoneId || undefined,
        kind,
        severity,
        priority,
        whiteboard: whiteboard || undefined,
        opSys,
        platform,
        url: url || undefined,
        confirmed: confirmed || undefined,
      },
      // Navigation is not refresh plumbing, so it stays. It runs after the
      // invalidation the mutation awaits, so the issue page opens over fresh
      // data rather than racing it.
      { onSuccess: (issue) => navigate(`/issues/${issue.id}`) },
    );
  };

  if (isVisitor(session)) {
    return (
      <div className="page">
        <h1>New issue</h1>
        <p className="state-hint">Filing an issue requires a signed-in identity. Sign in and reload.</p>
      </div>
    );
  }

  return (
    <div className="page new-issue-page">
      <h1>New issue</h1>
      <AsyncSection query={productsQ} loadingLabel="Loading products..." isEmpty={(d) => d.length === 0} emptyTitle="No products to file against.">
        {(products) => (
          <>
          {/* The shape of the form, up front.
              Only step 1 renders until a product is chosen, so the page opened
              as a single select on an empty screen: you could not tell whether
              two more fields were coming or ten, and the "1." implied a
              sequence with nothing to compare it to. */}
          <ol className="form-steps" aria-label="Filing steps">
            {[
              { n: 1, label: "Product", done: Boolean(productId) },
              { n: 2, label: "Component", done: Boolean(componentId) },
              { n: 3, label: "Details", done: false },
            ].map((step, index, all) => {
              const current = all.findIndex((s2) => !s2.done);
              const state = step.done ? "done" : index === current ? "current" : "todo";
              return (
                <li key={step.n} className={state} aria-current={state === "current" ? "step" : undefined}>
                  <span className="step-number">{step.n}</span>
                  {step.label}
                </li>
              );
            })}
          </ol>
          <form
            className="new-issue-form"
            onSubmit={(e) => {
              e.preventDefault();
              submit();
            }}
          >
            <Field>
              <Field.Label>Product</Field.Label>
              <Select
                value={productId}
                onValueChange={(next) => pickProduct(next ?? "")}
                placeholder="Select a product"
                aria-label="Product"
                renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}
              >
                {products.map((p) => (
                  <Select.Item key={p.id} value={p.id}>
                    {p.name}
                  </Select.Item>
                ))}
              </Select>
            </Field>
            {productQ.isError ? (
              <p className="field-error">{errorMessage(productQ.error)}</p>
            ) : null}

            {productDetail ? (
              <>
                <Field>
                  <Field.Label>Component</Field.Label>
                  <Select
                    value={componentId}
                    onValueChange={(next) => setComponentId(next ?? "")}
                    placeholder="Select a component"
                    aria-label="Component"
                    renderValue={(id) => productDetail.components.find((c) => c.id === id)?.name ?? id}
                  >
                    {productDetail.components.map((c) => (
                      <Select.Item key={c.id} value={c.id}>
                        {c.name}
                      </Select.Item>
                    ))}
                  </Select>
                </Field>

                <fieldset disabled={!componentId}>
                  <legend>Details</legend>
                  <Field>
                    <Field.Label>Summary</Field.Label>
                    <Input value={summary} onChange={(e) => setSummary(e.target.value)} maxLength={500} required />
                  </Field>
                  <Field>
                    <Field.Label htmlFor="new-issue-description">Description</Field.Label>
                    <textarea
                      className="app-field-shell app-textarea"
                      id="new-issue-description"
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                      rows={6}
                      required
                    />
                  </Field>
                  <div className="field-row">
                    <Field>
                      <Field.Label>Version</Field.Label>
                      {/* Optional, like Milestone beside it. "Version found in"
                          is a defect concept: a feature request is not found
                          in a version, so requiring one here made the form
                          unable to express half of what the tracker now holds.
                          `issues.versionId` is nullable for the same reason. */}
                      <Select
                        value={versionId}
                        onValueChange={(next) => setVersionId(next ?? "")}
                        placeholder="unspecified"
                        aria-label="Version"
                        renderValue={(id) => productDetail.versions.find((v) => v.id === id)?.name ?? id}
                      >
                        {productDetail.versions.map((v) => (
                          <Select.Item key={v.id} value={v.id}>
                            {v.name}
                          </Select.Item>
                        ))}
                      </Select>
                    </Field>
                    <Field>
                      <Field.Label>Milestone</Field.Label>
                      <Select
                        value={milestoneId}
                        onValueChange={(next) => setMilestoneId(next ?? "")}
                        placeholder="unspecified"
                        aria-label="Milestone"
                        renderValue={(id) =>
                          productDetail.milestones.find((m) => m.id === id)?.name ?? id
                        }
                      >
                        {productDetail.milestones.map((m) => (
                          <Select.Item key={m.id} value={m.id}>
                            {m.name}
                          </Select.Item>
                        ))}
                      </Select>
                    </Field>
                  </div>
                  <div className="field-row">
                    {/* Kind before Severity: "what is this" is the question
                        that decides whether the severity beside it is even
                        about a defect. It used to be answerable only by
                        picking `enhancement` as a SEVERITY, which is why a
                        critical feature request was unsayable. */}
                    <Field>
                      <Field.Label>Kind</Field.Label>
                      <Select
                        value={kind}
                        onValueChange={(next) => setKind(next as IssueKind)}
                        aria-label="Kind"
                      >
                        {ISSUE_KINDS.map((k) => (
                          <Select.Item key={k} value={k}>
                            {k}
                          </Select.Item>
                        ))}
                      </Select>
                    </Field>
                    <Field>
                      <Field.Label>Severity</Field.Label>
                      <Select
                        value={severity}
                        onValueChange={(next) => setSeverity(next as IssueSeverity)}
                        aria-label="Severity"
                      >
                        {ISSUE_SEVERITIES.map((s) => (
                          <Select.Item key={s} value={s}>
                            {s}
                          </Select.Item>
                        ))}
                      </Select>
                    </Field>
                    <Field>
                      <Field.Label>Priority</Field.Label>
                      <Select
                        value={priority}
                        onValueChange={(next) => setPriority(next as IssuePriority)}
                        aria-label="Priority"
                      >
                        {ISSUE_PRIORITIES.map((p) => (
                          <Select.Item key={p} value={p}>
                            {p}
                          </Select.Item>
                        ))}
                      </Select>
                    </Field>
                  </div>
                  <div className="field-row">
                    <Field>
                      <Field.Label>OS</Field.Label>
                      <Input value={opSys} onChange={(e) => setOpSys(e.target.value)} />
                    </Field>
                    <Field>
                      <Field.Label>Platform</Field.Label>
                      <Input value={platform} onChange={(e) => setPlatform(e.target.value)} />
                    </Field>
                  </div>
                  <Field>
                    <Field.Label>Whiteboard</Field.Label>
                    <Input value={whiteboard} onChange={(e) => setWhiteboard(e.target.value)} />
                  </Field>
                  <Field>
                    <Field.Label>URL</Field.Label>
                    <Input value={url} onChange={(e) => setUrl(e.target.value)} />
                  </Field>
                  {productDetail.product.allowsUnconfirmed ? (
                    <Checkbox
                      checked={confirmed}
                      onCheckedChange={(next) => setConfirmed(next === true)}
                      label="File as CONFIRMED (skip UNCONFIRMED)"
                    />
                  ) : (
                    <p className="state-hint small">
                      This product does not allow UNCONFIRMED issues; this will be filed as CONFIRMED.
                    </p>
                  )}

                  <Button
                    type="submit"
                    variant="filled"
                    disabled={create.isPending || !summary.trim() || !description.trim()}
                  >
                    {create.isPending ? "Filing..." : "File issue"}
                  </Button>
                  {create.error ? (
                    <p className="field-error">{errorMessage(create.error)}</p>
                  ) : null}
                </fieldset>
              </>
            ) : null}
          </form>
          </>
        )}
      </AsyncSection>
    </div>
  );
}
