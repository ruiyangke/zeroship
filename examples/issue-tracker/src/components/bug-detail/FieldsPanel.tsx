import { useState } from "react";
import { moveBug, reassignBug, setBugPriority, setBugSeverity, updateBug } from "../../api";
import { BUG_PRIORITIES, BUG_SEVERITIES, type BugPriority, type BugSeverity } from "../../lib/quicksearch";
import { PriorityBadge, SeverityBadge } from "../Badges";
import { errorMessage, toPromise } from "../rpc";
import type { BugDetail, ProductDetail } from "../types";
import { UserPicker } from "../UserPicker";
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
        Severity <SeverityBadge severity={bug.severity} />
        <select
          value={bug.severity}
          disabled={busy !== null}
          onChange={async (e) => {
            const severity = e.target.value as BugSeverity;
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
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
      </label>
      <label>
        Priority <PriorityBadge priority={bug.priority} />
        <select
          value={bug.priority}
          disabled={busy !== null}
          onChange={async (e) => {
            const priority = e.target.value as BugPriority;
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
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      </label>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

function AssigneeControl({ bug, onUpdated }: { bug: Bug; onUpdated: (b: Bug) => void }) {
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
        <span>{bug.assigneeId ?? "unassigned"}</span>
        <button type="button" className="btn ghost small" disabled={busy} onClick={() => setPicking((v) => !v)}>
          Change
        </button>
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
        <button type="button" className="btn ghost small" onClick={() => setOpen((v) => !v)}>
          {open ? "Cancel" : "Move to another product/component..."}
        </button>
      </div>
      {open ? (
        <div className="inline-form">
          <label>
            Target product
            <select value={targetProductId} onChange={(e) => void pickProduct(e.target.value)}>
              <option value="">Select a product</option>
              {products.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Target component
            <select
              value={targetComponentId}
              disabled={!components}
              onChange={(e) => setTargetComponentId(e.target.value)}
            >
              <option value="">Select a component</option>
              {components?.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            className="btn primary small"
            disabled={busy || !targetProductId || !targetComponentId}
            onClick={() => void move()}
          >
            Confirm move
          </button>
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
          <button type="button" className="btn ghost small" disabled={busy} onClick={() => void save()}>
            Save
          </button>
        ) : null}
      </span>
      {error ? <p className="field-error">{error}</p> : null}
    </label>
  );
}

export function FieldsPanel({
  bug,
  product,
  component,
  productDetail,
  products,
  fetchProductDetail,
  onUpdated,
}: {
  bug: Bug;
  product: BugProduct;
  component: BugComponent;
  productDetail: ProductDetail | null;
  products: { id: string; name: string }[];
  fetchProductDetail: (id: string) => Promise<ProductDetail>;
  onUpdated: (b: Bug) => void;
}) {
  const [versionMilestoneError, setVersionMilestoneError] = useState<string | null>(null);
  return (
    <section className="fields-panel">
      <h2>{bug.summary}</h2>
      <GeneralField bug={bug} field="summary" label="Summary" value={bug.summary} onUpdated={onUpdated} />

      <StatusControl bug={bug} onUpdated={onUpdated} />
      <SeverityPriority bug={bug} onUpdated={onUpdated} />
      <AssigneeControl bug={bug} onUpdated={onUpdated} />

      <div className="field-row readonly">
        <span>
          Product: <b>{product.name}</b>
        </span>
        <span>
          Component: <b>{component.name}</b>
        </span>
        <span>Reporter: {bug.reporterId}</span>
        <span>QA contact: {bug.qaContactId ?? "--"}</span>
      </div>

      <MoveControl bug={bug} products={products} onUpdated={onUpdated} fetchProductDetail={fetchProductDetail} />

      {productDetail ? (
        <div className="field-row">
          <label>
            Version
            <select
              value={bug.versionId ?? ""}
              onChange={(e) => {
                if (!e.target.value) return;
                setVersionMilestoneError(null);
                toPromise(updateBug({ id: bug.id, changes: { versionId: e.target.value } }))
                  .then(onUpdated)
                  .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
              }}
            >
              <option value="">unspecified</option>
              {productDetail.versions.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Milestone
            <select
              value={bug.milestoneId ?? ""}
              onChange={(e) => {
                if (!e.target.value) return;
                setVersionMilestoneError(null);
                toPromise(updateBug({ id: bug.id, changes: { milestoneId: e.target.value } }))
                  .then(onUpdated)
                  .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
              }}
            >
              <option value="">unspecified</option>
              {productDetail.milestones.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </select>
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
    </section>
  );
}
