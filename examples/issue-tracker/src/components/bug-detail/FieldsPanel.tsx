import { useState } from "react";
import { Button, Card, DescriptionList, Select } from "@zeroship/ui";
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
      <label>
        {/* The badge is gone. It sat directly above a select showing the same
            value, so the page stated severity twice and neither told you which
            one to use. */}
        Severity
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
      </label>
      <label>
        Priority
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
      </label>
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
          {open ? "Cancel" : "Move to another product/component..."}
        </Button>
      </div>
      {open ? (
        <div className="inline-form">
          <label>
            Target product
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
          </label>
          <label>
            Target component
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
          </label>
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
  const [draft, setDraft] = useState(value);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dirty = draft !== value;

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await updateBug({ id: bug.id, changes: { [field]: draft } }));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="field-block">
      <span className="field-label">{label}</span>
      <span className="field-block-head">
        <input value={draft} disabled={busy} onChange={(e) => setDraft(e.target.value)} />
        {dirty ? (
          <Button variant="gray" size="small" disabled={busy} onClick={() => void save()}>
            Save
          </Button>
        ) : null}
      </span>
      {error ? <p className="field-error">{error}</p> : null}
    </label>
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
}: {
  bug: Bug;
  people: PeopleMap;
  product: BugProduct;
  component: BugComponent;
  productDetail: ProductDetail | null;
  products: { id: string; name: string }[];
  fetchProductDetail: (id: string) => Promise<ProductDetail>;
  onUpdated: (b: Bug) => void;
}) {
  const [versionMilestoneError, setVersionMilestoneError] = useState<string | null>(null);
  return (
    <Card className="fields-panel">
      {/* No heading here. The page head already states the summary, and this
          panel repeated it as an h2 immediately above an input containing the
          same text -- the same string three times in the top 200 pixels. */}
      <GeneralField bug={bug} field="summary" label="Summary" value={bug.summary} onUpdated={onUpdated} />

      <StatusControl bug={bug} onUpdated={onUpdated} />
      <SeverityPriority bug={bug} onUpdated={onUpdated} />
      <AssigneeControl bug={bug} people={people} onUpdated={onUpdated} />

      {/* The facts you read rather than change, as a description list. They
          were four spans in a row with hand-rolled "Label: value" strings and
          inconsistent emphasis -- two bold values, two not. */}
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
          <label>
            Version
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
              placeholder="unspecified"
              renderValue={(id) => productDetail.versions.find((v) => v.id === id)?.name ?? id}
            >
              {productDetail.versions.map((v) => (
                <Select.Item key={v.id} value={v.id}>
                  {v.name}
                </Select.Item>
              ))}
            </Select>
          </label>
          <label>
            Milestone
            <Select
              value={bug.milestoneId ?? ""}
              onValueChange={(next) => {
                if (!next) return;
                setVersionMilestoneError(null);
                toPromise(updateBug({ id: bug.id, changes: { milestoneId: next } }))
                  .then(onUpdated)
                  .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
              }}
              placeholder="unspecified"
              renderValue={(id) => productDetail.milestones.find((m) => m.id === id)?.name ?? id}
            >
              {productDetail.milestones.map((m) => (
                <Select.Item key={m.id} value={m.id}>
                  {m.name}
                </Select.Item>
              ))}
            </Select>
          </label>
        </div>
      ) : (
        <p className="state-hint small">Sign in to change version/milestone.</p>
      )}
      {versionMilestoneError ? <p className="field-error">{versionMilestoneError}</p> : null}

      <GeneralField bug={bug} field="whiteboard" label="Whiteboard" value={bug.whiteboard ?? ""} onUpdated={onUpdated} />
      <div className="field-row">
        <GeneralField bug={bug} field="opSys" label="OS" value={bug.opSys} onUpdated={onUpdated} />
        <GeneralField bug={bug} field="platform" label="Platform" value={bug.platform} onUpdated={onUpdated} />
      </div>
      <GeneralField bug={bug} field="url" label="URL" value={bug.url ?? ""} onUpdated={onUpdated} />
    </Card>
  );
}
