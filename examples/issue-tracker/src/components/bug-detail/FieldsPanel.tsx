import { useState } from "react";
import { Button, Cluster, DescriptionList, Field, Input, Select } from "@zeroship/ui";
import { moveBug, reassignBug, setBugPriority, setBugSeverity, updateBug } from "../../api";
import { BUG_PRIORITIES, BUG_SEVERITIES, type BugPriority, type BugSeverity } from "../../lib/quicksearch";

import { PriorityBadge, SeverityBadge } from "../Badges";
import { RailChoice } from "./RailChoice";
import { RailProperty } from "./RailProperty";
import { errorMessage, toPromise } from "../rpc";
import type { BugDetail, ProductDetail } from "../types";
import { UserPicker } from "../UserPicker";
import { personName, type PeopleMap } from "./people";
import { StatusControl } from "./StatusControl";
import { Absent } from "./Absent";

type Bug = BugDetail["bug"];
type BugProduct = NonNullable<BugDetail["product"]>;
type BugComponent = NonNullable<BugDetail["component"]>;

function SeverityPriority({ bug, onUpdated }: { bug: Bug; onUpdated: (b: Bug) => void }) {
  const [busy, setBusy] = useState<"severity" | "priority" | null>(null);
  const [error, setError] = useState<string | null>(null);

  return (
    <>
      <RailChoice
        label="Severity"
        value={bug.severity}
        display={<SeverityBadge severity={bug.severity} />}
        disabled={busy !== null}
        options={BUG_SEVERITIES.map((value) => ({ value, label: value }))}
        onChange={async (next) => {
          setBusy("severity");
          setError(null);
          try {
            onUpdated(await setBugSeverity({ id: bug.id, severity: next as BugSeverity }));
          } catch (err) {
            setError(errorMessage(err));
          } finally {
            setBusy(null);
          }
        }}
      />
      <RailChoice
        label="Priority"
        value={bug.priority}
        display={<PriorityBadge priority={bug.priority} />}
        disabled={busy !== null}
        options={BUG_PRIORITIES.map((value) => ({ value, label: value }))}
        onChange={async (next) => {
          setBusy("priority");
          setError(null);
          try {
            onUpdated(await setBugPriority({ id: bug.id, priority: next as BugPriority }));
          } catch (err) {
            setError(errorMessage(err));
          } finally {
            setBusy(null);
          }
        }}
      />
      {error ? <p className="field-error">{error}</p> : null}
    </>
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

  const assign = async (assigneeId: string, done: () => void) => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await reassignBug({ id: bug.id, assigneeId }));
      done();
    } catch (err) {
      // Deliberately stays open on failure: closing would discard the choice
      // and leave the old name showing, which reads as a refusal nobody made.
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <RailProperty
        label="Assignee"
        display={bug.assigneeId ? personName(bug.assigneeId, people) : <Absent />}
        disabled={busy}
        wide
      >
        {(done) => <UserPicker onPick={(user) => void assign(user.id, done)} />}
      </RailProperty>
      {error ? <p className="field-error">{error}</p> : null}
    </>
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
      {/* No label. Every other rail line is "property: value"; this one had
          no value to state, so hiding its button until hover left the word
          "Move" sitting alone like a field whose contents had gone missing.
          It is an action, and it is written as one. */}
      <div className="field-block-head is-action">
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
        <span className="field-value">{value ? value : <Absent />}</span>
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
  const [showOther, setShowOther] = useState(false);
  // "Unspecified" is the server's default for opSys and platform, so a bug
  // that has never been touched reads as unset rather than as configured.
  const hasOther = Boolean(
    bug.whiteboard ||
      bug.url ||
      (bug.opSys && bug.opSys !== "Unspecified") ||
      (bug.platform && bug.platform !== "Unspecified"),
  );
  return (
    <div className="fields-panel">
      <fieldset className="rail-fields" disabled={readOnly}>
      {/* The summary is edited in the PAGE HEAD, where the summary is.
          It lived here as "Edit summary" floating at the top of the rail, a
          control several hundred pixels from the words it changes and in a
          column reserved for metadata. A title is not metadata about itself. */}
      {/* Not a permanent text box. The page heading already states the
          summary in full; a second copy in a 22rem rail truncated it and
          invited edits nobody came to make. Editing is a deliberate act. */}
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
          <DescriptionList.Detail>
            {bug.qaContactId ? personName(bug.qaContactId, people) : <Absent />}
          </DescriptionList.Detail>
        </DescriptionList.Item>
      </DescriptionList>

      {productDetail ? (
        <>
          {/* Read-first, like every other property here. These were the last
              two bordered selects in the rail, so the column still had two
              rhythms: facts you read, and controls you fill in. */}
          <RailChoice
            label="Version"
            value={bug.versionId ?? ""}
            display={
              productDetail.versions.find((v) => v.id === bug.versionId)?.name ?? (
                <Absent />
              )
            }
            options={productDetail.versions.map((v) => ({ value: v.id, label: v.name }))}
            onChange={(next) => {
              setVersionMilestoneError(null);
              toPromise(updateBug({ id: bug.id, changes: { versionId: next } }))
                .then(onUpdated)
                .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
            }}
          />
          <RailChoice
            label="Milestone"
            value={bug.milestoneId ?? ""}
            display={
              productDetail.milestones.find((m) => m.id === bug.milestoneId)?.name ?? (
                <Absent />
              )
            }
            options={productDetail.milestones.map((m) => ({ value: m.id, label: m.name }))}
            onChange={(next) => {
              setVersionMilestoneError(null);
              toPromise(updateBug({ id: bug.id, changes: { milestoneId: next } }))
                .then(onUpdated)
                .catch((err: unknown) => setVersionMilestoneError(errorMessage(err)));
            }}
          />
        </>
      ) : (
        <p className="state-hint small">Sign in to change version/milestone.</p>
      )}
      {versionMilestoneError ? <p className="field-error">{versionMilestoneError}</p> : null}

      {/* After the properties, not among them. It sat between "QA contact"
          and "Version", so a column of "label: value" lines was interrupted
          by a lone button and the reader lost the rhythm mid-scan. Moving a
          bug changes product AND component, so it belongs to the whole group
          rather than to any one line in it. */}
      <MoveControl bug={bug} products={products} onUpdated={onUpdated} fetchProductDetail={fetchProductDetail} />

      {/* Four rows that usually say nothing.
          Whiteboard, OS, platform and URL are unset on most bugs, so the rail
          ended with "Whiteboard --", "OS Unspecified", "Platform Unspecified",
          "URL --" -- four lines of absence at the bottom of a column whose job
          is to be glanceable. They appear when they HAVE a value, and behind
          one line when they do not, so setting them is still one click and
          reading a bug that never used them costs nothing. */}
      <RailSection title="Other" />
      {hasOther || showOther ? (
        <>
          <GeneralField bug={bug} field="whiteboard" label="Whiteboard" value={bug.whiteboard ?? ""} onUpdated={onUpdated} />
          <div className="field-row">
            <GeneralField bug={bug} field="opSys" label="OS" value={bug.opSys} onUpdated={onUpdated} />
            <GeneralField bug={bug} field="platform" label="Platform" value={bug.platform} onUpdated={onUpdated} />
          </div>
          <GeneralField bug={bug} field="url" label="URL" value={bug.url ?? ""} onUpdated={onUpdated} />
        </>
      ) : (
        <Button variant="plain" size="small" onClick={() => setShowOther(true)}>
          Set whiteboard, OS, platform or URL
        </Button>
      )}
      </fieldset>
    </div>
  );
}
