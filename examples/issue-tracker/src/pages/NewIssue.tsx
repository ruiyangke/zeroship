import { useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { Button, Checkbox, Field, Input, Select } from "@zeroship/ui";
import { createIssue } from "../api";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useProduct, useProducts } from "../lib/queries";
import { AsyncSection } from "../components/StateViews";
import { AppFieldShell } from "../components/AppFieldShell";
import { FieldError, FieldRow, Hint, Page } from "../components/AppPrimitives";
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

type FilingStepState = "current" | "done" | "todo";

function FilingStep({
  number,
  state,
  children,
}: {
  number: number;
  state: FilingStepState;
  children: ReactNode;
}) {
  return (
    <li
      className="group flex items-center gap-2 text-ink-muted data-[state=current]:font-semibold data-[state=current]:text-ink data-[state=done]:text-ink-secondary"
      data-state={state}
      aria-current={state === "current" ? "step" : undefined}
    >
      <span className="inline-flex size-6 items-center justify-center rounded-full border border-line-strong text-sm font-semibold group-data-[state=current]:border-accent-strong group-data-[state=current]:text-accent-strong group-data-[state=done]:border-accent-strong group-data-[state=done]:bg-accent-soft group-data-[state=done]:text-accent-strong">
        {number}
      </span>
      {children}
    </li>
  );
}

function FilingSteps({
  productChosen,
  componentChosen,
}: {
  productChosen: boolean;
  componentChosen: boolean;
}) {
  const steps = [
    { number: 1, label: "Product", done: productChosen },
    { number: 2, label: "Component", done: componentChosen },
    { number: 3, label: "Details", done: false },
  ];
  const current = steps.findIndex((step) => !step.done);

  return (
    <ol
      className="mb-4 flex max-w-160 list-none flex-wrap gap-5 p-0 text-base text-ink-muted"
      aria-label="Filing steps"
    >
      {steps.map((step, index) => {
        const state: FilingStepState = step.done ? "done" : index === current ? "current" : "todo";
        return (
          <FilingStep key={step.number} number={step.number} state={state}>
            {step.label}
          </FilingStep>
        );
      })}
    </ol>
  );
}

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
      <Page>
        <h1 className="text-xl font-semibold tracking-[-0.01em]">New issue</h1>
        <Hint>Filing an issue requires a signed-in identity. Sign in and reload.</Hint>
      </Page>
    );
  }

  return (
    <Page className="mx-auto max-w-160">
      <h1 className="text-xl font-semibold tracking-[-0.01em]">New issue</h1>
      <AsyncSection
        query={productsQ}
        loadingLabel="Loading products..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No products to file against."
      >
        {(products) => (
          <>
            {/* The shape of the form, up front.
                Only step 1 renders until a product is chosen, so the page opened
                as a single select on an empty screen: you could not tell whether
                two more fields were coming or ten, and the "1." implied a
                sequence with nothing to compare it to. */}
            <FilingSteps
              productChosen={Boolean(productId)}
              componentChosen={Boolean(componentId)}
            />
            <form
              className="flex flex-col gap-3 rounded-lg border border-line bg-surface p-4"
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
                <FieldError>{errorMessage(productQ.error)}</FieldError>
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

                  <fieldset
                    className="my-2 flex flex-col gap-3 rounded-lg border border-dashed border-line-strong p-4 disabled:opacity-50"
                    disabled={!componentId}
                  >
                    <legend className="px-2 text-base font-semibold text-ink-secondary">Details</legend>
                    <Field>
                      <Field.Label>Summary</Field.Label>
                      <Input value={summary} onChange={(e) => setSummary(e.target.value)} maxLength={500} required />
                    </Field>
                    <Field>
                      <Field.Label htmlFor="new-issue-description">Description</Field.Label>
                      <AppFieldShell
                        as="textarea"
                        className="app-textarea w-full px-2 py-2 font-[inherit] text-md outline-none"
                        id="new-issue-description"
                        value={description}
                        onChange={(e) => setDescription(e.target.value)}
                        rows={6}
                        required
                      />
                    </Field>
                    <FieldRow>
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
                    </FieldRow>
                    <FieldRow>
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
                    </FieldRow>
                    <FieldRow>
                      <Field>
                        <Field.Label>OS</Field.Label>
                        <Input value={opSys} onChange={(e) => setOpSys(e.target.value)} />
                      </Field>
                      <Field>
                        <Field.Label>Platform</Field.Label>
                        <Input value={platform} onChange={(e) => setPlatform(e.target.value)} />
                      </Field>
                    </FieldRow>
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
                      <Hint className="text-sm">
                        This product does not allow UNCONFIRMED issues; this will be filed as CONFIRMED.
                      </Hint>
                    )}

                    <Button
                      type="submit"
                      variant="filled"
                      disabled={create.isPending || !summary.trim() || !description.trim()}
                    >
                      {create.isPending ? "Filing..." : "File issue"}
                    </Button>
                    {create.error ? (
                      <FieldError>{errorMessage(create.error)}</FieldError>
                    ) : null}
                  </fieldset>
                </>
              ) : null}
            </form>
          </>
        )}
      </AsyncSection>
    </Page>
  );
}
