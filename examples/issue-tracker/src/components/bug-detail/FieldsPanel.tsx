import { useState } from "react";
import { Button, Card, Cluster, DescriptionList, Field, Input, Select } from "@zeroship/ui";
import { moveBug, reassignBug, setBugPriority, setBugSeverity, updateBug } from "../../api";
import { BUG_PRIORITIES, BUG_SEVERITIES, type BugPriority, type BugSeverity } from "../../lib/quicksearch";

import { errorMessage, toPromise } from "../rpc";
import type { BugDetail, ProductDetail } from "../types";
import { UserPicker } from "../UserPicker";
import { personName, type PeopleMap } from "./people";
import { StatusControl } from "./StatusControl";

type Bug = BugDetail["bug"];
type BugProduct = NonNullable<BugDetail["product"]>;
type BugComponent = NonNullable<BugDetail["component"]>;

function SeverityPriority({ bug, onUpdated }: { bug: Bug; onUpdated: (b: Bug) => void }) {
  const [busy, setBusy] = useState<"severity" | "priority" | null>(null);
  const [error, setError] = useState<string | null>(null);

  return (
    <div className="field-row">
      <Field orientation="horizontal">
        {/* The badge is gone. It sat directly above a select showing the same
            value, so the page stated severity twice and neither told you which
            one to use. */}
        <Field.Label>Severity</Field.Label>
        <Select
          value={bug.severity}
          disabled={busy !== null}
          aria-label="Severity"
          onValueChange={async (next) => {
            const severity = next as BugSeverity;
            setBusy("severity");
            setError(null);
            try {
              onUpdated(await setBugSeverity({ id: bug.id, severity }));
            } catch (err) {
              setError(errorMessage(err));
            } finally {
              setBusy(null);
            }
          }}
        >
          {BUG_SEVERITIES.map((s) => (
            <Select.Item key={s} value={s}>
              {s}
            </Select.Item>
          ))}
        </Select>
      </Field>
      <Field orientation="horizontal">
        <Field.Label>Priority</Field.Label>
        <Select
          value={bug.priority}
          disabled={busy !== null}
          aria-label="Priority"
          onValueChange={async (next) => {
            const priority = next as BugPriority;
            setBusy("priority");
            setError(null);
            try {
              onUpdated(await setBugPriority({ id: bug.id, priority }));
            } catch (err) {
              setError(errorMessage(err));
            } finally {
              setBusy(null);
            }
          }}
        >
          {BUG_PRIORITIES.map((p) => (
            <Select.Item key={p} value={p}>
              {p}
            </Select.Item>
          ))}
        </Select>
      </Field>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

function AssigneeControl({
  bug,
  people,
  onUpdated,
}: {
  bug: Bug;
  people: PeopleMap;
  onUpdated: (b: Bug) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const assign = async (assigneeId: string) => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await reassignBug({ id: bug.id, assigneeId }));
      setPicking(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="field-block">
      <div className="field-block-head">
        <span className="field-label">Assignee</span>
        <span>{personName(bug.assigneeId, people, "unassigned")}</span>
        <Button variant="gray" size="small" disabled={busy} onClick={() => setPicking((v) => !v)}>
          Change
        </Button>
      </div>
      {picking ? <UserPicker onPick={(user) => void assign(user.id)} /> : null}
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

function MoveControl({
  bug,
  products,
  onUpdated,
  fetchProductDetail,
}: {
  bug: Bug;
  products: { id: string; name: string }[];
  onUpdated: (b: Bug) => void;
  fetchProductDetail: (id: string) => Promise<ProductDetail>;
}) {
  const [open, setOpen] = useState(false);
  const [targetProductId, setTargetProductId] = useState("");
  const [components, setComponents] = useState<ProductDetail["components"] | null>(null);
  const [targetComponentId, setTargetComponentId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pickProduct = async (id: string) => {
    setTargetProductId(id);
    setTargetComponentId("");
    setComponents(null);
    if (!id) return;
    try {
      const detail = await fetchProductDetail(id);
      setComponents(detail.components);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const move = async () => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await moveBug({ id: bug.id, productId: targetProductId, componentId: targetComponentId }));
      setOpen(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="field-block">
      <div className="field-block-head">
        <span className="field-label">Move</span>
        <Button variant="gray" size="small" onClick={() => setOpen((v) => !v)}>
          {/* Short enough to fit the rail. The full sentence ran past the
              column edge, which is how a rail says "this control does not
              belong here" -- the dialog it opens explains the rest. */}
          {open ? "Cancel" : "Move bug..."}
        </Button>
      </div>
      {open ? (
        <div className="inline-form">
          <Field orientation="horizontal">
            <Field.Label>Target product</Field.Label>
            <Select
              value={targetProductId}
              onValueChange={(next) => void pickProduct(next ?? "")}
              placeholder="Select a product"
              aria-label="Target product"
              renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}
            >
              {products.map((p) => (
                <Select.Item key={p.id} value={p.id}>
                  {p.name}
                </Select.Item>
              ))}
            </Select>
          </Field>
          <Field orientation="horizontal">
            <Field.Label>Target component</Field.Label>
            <Select
              value={targetComponentId}
              disabled={!components}
              onValueChange={(next) => setTargetComponentId(next ?? "")}
              placeholder="Select a component"
              aria-label="Target component"
              renderValue={(id) => components?.find((c) => c.id === id)?.name ?? id}
            >
              {components?.map((c) => (
                <Select.Item key={c.id} value={c.id}>
                  {c.name}
                </Select.Item>
              ))}
            </Select>
          </Field>
          <Button variant="filled" size="small"
            disabled={busy || !targetProductId || !targetComponentId}
            onClick={() => void move()}
          >
            Confirm move
          </Button>
          <p className="state-hint small">
            Moving a bug that already has a version or milestone set is rejected by the
            server today (it cannot clear those fields).
          </p>
        </div>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

/**
 * A labelled divider between groups of rail fields.
 *
 * The rail was one uninterrupted column of thirteen controls, so status,
 * classification, people and location all had the same standing and the eye
 * had nowhere to rest. These are the joints.
 */
function RailSection({ title }: { title: string }) {
  return <p className="rail-section">{title}</p>;
}

function GeneralField({
  bug,
  field,
  label,
  value,
  onUpdated,
}: {
  bug: Bug;
  field: "summary" | "whiteboard" | "opSys" | "platform" | "url";
  label: string;
  value: string;
  onUpdated: (b: Bug) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await updateBug({ id: bug.id, changes: { [field]: draft } }));
      setEditing(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  // Read until asked. Four of these -- whiteboard, OS, platform, URL -- sat
  // open as text inputs in a 352px rail, so the column that should let you
  // GLANCE at a bug's metadata was mostly empty edit boxes for fields almost
  // nobody sets. A value you are not changing is something to read.
  if (!editing) {
    return (
      <div className="field-row-compact">
        <span className="field-label">{label}</span>
        <span className="field-value">{value || "--"}</span>
        <Button
          variant="plain"
          size="small"
          // Named per field: a rail with five bare "Edit" buttons is
          // ambiguous to a screen reader and to a strict-mode locator.
          aria-label={`Edit ${label}`}
          onClick={() => {
            setDraft(value);
            setEditing(true);
          }}
        >
          Edit
        </Button>
      </div>
    );
  }

  return (
    <Field orientation="horizontal" className="field-block">
      <Field.Label>{label}</Field.Label>
      <span className="field-block-head">
        <Input value={draft} disabled={busy} onChange={(e) => setDraft(e.target.value)} />
        <Button variant="gray" size="small" disabled={busy} onClick={() => void save()}>
          Save
        </Button>
        <Button
          variant="plain"
          size="small"
          disabled={busy}
          aria-label={`Cancel editing ${label}`}
          onClick={() => {
            setDraft(value);
            setEditing(false);
            setError(null);
          }}
        >
          Cancel
        </Button>
      </span>
      {error ? <p className="field-error">{error}</p> : null}
    </Field>
  );
}

export function FieldsPanel({
  bug,
  people,
  product,
  component,
  productDetail,
  products,
  fetchProductDetail,
  onUpdated,
  readOnly = false,
}: {
  bug: Bug;
  people: PeopleMap;
  product: BugProduct;
  component: BugComponent;
  productDetail: ProductDetail | null;
  products: { id: string; name: string }[];
  fetchProductDetail: (id: string) => Promise<ProductDetail>;
  onUpdated: (b: Bug) => void;
  /**
   * No identity: every control here would 401 on use, so none is offered.
   *
   * A native <fieldset disabled> rather than a flag threaded into each of the
   * dozen controls below. The browser disables every form control inside it,
   * including ones added later, so a field added next month is covered
   * without anyone remembering -- the same reason the panel chrome is
   * selected structurally rather than by a list of class names.
   */
  readOnly?: boolean;
}) {
  const [versionMilestoneError, setVersionMilestoneError] = useState<string | null>(null);
  const [editingSummary, setEditingSummary] = useState(false);
  return (
    <Card className="fields-panel">
      <fieldset className="rail-fields" disabled={readOnly}>
      {/* No heading here. The page head already states the summary, and this
          panel repeated it as an h2 immediately above an input containing the
          same text -- the same string three times in the top 200 pixels. */}
      {/* Not a permanent text box. The page heading already states the
          summary in full; a second copy in a 22rem rail truncated it and
          invited edits nobody came to make. Editing is a deliberate act. */}
      {editingSummary ? (
        <GeneralField
          bug={bug}
          field="summary"
          label="Summary"
          value={bug.summary}
          onUpdated={(next) => {
            setEditingSummary(false);
            onUpdated(next);
          }}
        />
      ) : (
        /* "Summary / Edit" alone at the top of the rail read as a field whose
           value had gone missing -- a label and a button with nothing between
           them. It is an action, so it is written as one. */
        <Cluster gap={2} align="center" justify="end" className="rail-action">
          <Button variant="gray" size="small" onClick={() => setEditingSummary(true)}>
            Edit summary
          </Button>
        </Cluster>
      )}

      <StatusControl bug={bug} onUpdated={onUpdated} />
      <RailSection title="Classification" />
      <SeverityPriority bug={bug} onUpdated={onUpdated} />
      <AssigneeControl bug={bug} people={people} onUpdated={onUpdated} />

      {/* The facts you read rather than change, as a description list. They
          were four spans in a row with hand-rolled "Label: value" strings and
          inconsistent emphasis -- two bold values, two not. */}
      <RailSection title="Where" />
      <DescriptionList>
        <DescriptionList.Item>
          <DescriptionList.Term>Product</DescriptionList.Term>
          <DescriptionList.Detail>{product.name}</DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>Component</DescriptionList.Term>
          <DescriptionList.Detail>{component.name}</DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>Reporter</DescriptionList.Term>
          <DescriptionList.Detail>{personName(bug.reporterId, people)}</DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>QA contact</DescriptionList.Term>
          <DescriptionList.Detail>{personName(bug.qaContactId, people)}</DescriptionList.Detail>
        </DescriptionList.Item>
      </DescriptionList>

      <MoveControl bug={bug} products={products} onUpdated={onUpdated} fetchProductDetail={fetchProductDetail} />

      {productDetail ? (
        <div className="field-row">
          <Field orientation="horizontal">
            <Field.Label>Version</Field.Label>
            {/* The design system Select, so the detail page stops mixing two
                kinds of dropdown with the filter bar. */}
            <Select
              value={bug.versionId ?? ""}
              onValueChange={(next) => {
                if (!next) return;
                setVersionMilestoneError(null);
                toPromise(updateBug({ id: bug.id, changes: { versionId: next } }))
                  .then(onUpdated)
                  .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
              }}
              placeholder="None"
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
          <Field orientation="horizontal">
            <Field.Label>Milestone</Field.Label>
            <Select
              value={bug.milestoneId ?? ""}
              onValueChange={(next) => {
                if (!next) return;
                setVersionMilestoneError(null);
                toPromise(updateBug({ id: bug.id, changes: { milestoneId: next } }))
                  .then(onUpdated)
                  .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
              }}
              placeholder="None"
              aria-label="Milestone"
              renderValue={(id) => productDetail.milestones.find((m) => m.id === id)?.name ?? id}
            >
              {productDetail.milestones.map((m) => (
                <Select.Item key={m.id} value={m.id}>
                  {m.name}
                </Select.Item>
              ))}
            </Select>
          </Field>
        </div>
      ) : (
        <p className="state-hint small">Sign in to change version/milestone.</p>
      )}
      {versionMilestoneError ? <p className="field-error">{versionMilestoneError}</p> : null}

      <RailSection title="Other" />
      <GeneralField bug={bug} field="whiteboard" label="Whiteboard" value={bug.whiteboard ?? ""} onUpdated={onUpdated} />
      <div className="field-row">
        <GeneralField bug={bug} field="opSys" label="OS" value={bug.opSys} onUpdated={onUpdated} />
        <GeneralField bug={bug} field="platform" label="Platform" value={bug.platform} onUpdated={onUpdated} />
      </div>
      <GeneralField bug={bug} field="url" label="URL" value={bug.url ?? ""} onUpdated={onUpdated} />
      </fieldset>
    </Card>
  );
}
