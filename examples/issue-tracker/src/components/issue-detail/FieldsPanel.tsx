import { useState } from "react";
import { Button, DescriptionList, Field, Input, Select } from "@zeroship/ui";
import {
  moveIssue,
  reassignIssue,
  setIssueKind,
  setIssuePriority,
  setIssueSeverity,
  updateIssue,
} from "../../api";
import {
  ISSUE_KINDS,
  ISSUE_PRIORITIES,
  ISSUE_SEVERITIES,
  type IssueKind,
  type IssuePriority,
  type IssueSeverity,
} from "../../lib/quicksearch";
import { invalidatedBy } from "../../lib/query-keys";
import { useAppMutation, useProduct } from "../../lib/queries";

import { KindBadge, PriorityBadge, SeverityBadge } from "../Badges";
import { RailChoice } from "./RailChoice";
import { RailProperty } from "./RailProperty";
import { errorMessage } from "../rpc";
import type { IssueDetail, ProductDetail } from "../types";
import { UserPicker } from "../UserPicker";
import { personName, type PeopleMap } from "./people";
import { RailSection } from "./RailSection";
import { RailFieldset } from "./RailFieldset";
import { StatusControl } from "./StatusControl";
import { Absent } from "./Absent";

type Issue = IssueDetail["issue"];
type IssueProduct = NonNullable<IssueDetail["product"]>;
type IssueComponent = NonNullable<IssueDetail["component"]>;

/**
 * Kind, severity and priority: what it is, how bad it is, when we do it.
 *
 * Kind leads because it is the question the other two are read against.
 * Severity used to carry `enhancement`, so the rail could say EITHER what a
 * row was or how much it hurt, never both -- a critical feature request had
 * to pick one. These are three independent lines now.
 */
function Classification({ issue }: { issue: Issue }) {
  const [error, setError] = useState<string | null>(null);
  const changeKind = useAppMutation(
    (kind: IssueKind) => setIssueKind({ id: issue.id, kind }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const changeSeverity = useAppMutation(
    (severity: IssueSeverity) => setIssueSeverity({ id: issue.id, severity }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const changePriority = useAppMutation(
    (priority: IssuePriority) => setIssuePriority({ id: issue.id, priority }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy = changeKind.isPending || changeSeverity.isPending || changePriority.isPending;

  return (
    <>
      <RailChoice
        label="Kind"
        value={issue.kind}
        display={<KindBadge kind={issue.kind} />}
        disabled={busy}
        options={ISSUE_KINDS.map((value) => ({ value, label: value }))}
        onChange={async (next) => {
          setError(null);
          try {
            await changeKind.mutateAsync(next as IssueKind);
          } catch (err) {
            setError(errorMessage(err));
          }
        }}
      />
      <RailChoice
        label="Severity"
        value={issue.severity}
        display={<SeverityBadge severity={issue.severity} />}
        disabled={busy}
        options={ISSUE_SEVERITIES.map((value) => ({ value, label: value }))}
        onChange={async (next) => {
          setError(null);
          try {
            await changeSeverity.mutateAsync(next as IssueSeverity);
          } catch (err) {
            setError(errorMessage(err));
          }
        }}
      />
      <RailChoice
        label="Priority"
        value={issue.priority}
        display={<PriorityBadge priority={issue.priority} />}
        disabled={busy}
        options={ISSUE_PRIORITIES.map((value) => ({ value, label: value }))}
        onChange={async (next) => {
          setError(null);
          try {
            await changePriority.mutateAsync(next as IssuePriority);
          } catch (err) {
            setError(errorMessage(err));
          }
        }}
      />
      {error ? <p className="field-error">{error}</p> : null}
    </>
  );
}

function AssigneeControl({
  issue,
  people,
}: {
  issue: Issue;
  people: PeopleMap;
}) {
  const [error, setError] = useState<string | null>(null);
  const reassign = useAppMutation(
    (assigneeId: string) => reassignIssue({ id: issue.id, assigneeId }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy = reassign.isPending;

  const assign = async (assigneeId: string, done: () => void) => {
    setError(null);
    try {
      await reassign.mutateAsync(assigneeId);
      // Closing the picker is a UI decision this control still owns. Only
      // the refresh moved into the mutation's invalidation.
      done();
    } catch (err) {
      // Deliberately stays open on failure: closing would discard the choice
      // and leave the old name showing, which reads as a refusal nobody made.
      setError(errorMessage(err));
    }
  };

  return (
    <>
      <RailProperty
        label="Assignee"
        display={issue.assigneeId ? personName(issue.assigneeId, people) : <Absent />}
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
  issue,
  products,
}: {
  issue: Issue;
  products: { id: string; name: string }[];
}) {
  const [open, setOpen] = useState(false);
  const [targetProductId, setTargetProductId] = useState("");
  const [targetComponentId, setTargetComponentId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const targetProductQ = useProduct(targetProductId || null);
  const components = targetProductQ.data?.components ?? null;
  const moveIssueToProduct = useAppMutation(
    ({ productId, componentId }: { productId: string; componentId: string }) =>
      moveIssue({ id: issue.id, productId, componentId }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy = moveIssueToProduct.isPending;

  const pickProduct = (id: string) => {
    setTargetProductId(id);
    setTargetComponentId("");
  };

  const move = async () => {
    setError(null);
    try {
      await moveIssueToProduct.mutateAsync({
        productId: targetProductId,
        componentId: targetComponentId,
      });
      // The dialog still closes here because that is local UI state. The
      // issue and report refreshes are owned by the mutation above.
      setOpen(false);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <div className="field-block">
      {/* No label. Every other rail line is "property: value"; this one had
          no value to state, so hiding its button until hover left the word
          "Move" sitting alone like a field whose contents had gone missing.
          It is an action, and it is written as one. */}
      <span aria-hidden="true" />
      <div className="field-block-head is-action">
        <Button variant="gray" size="sm" onClick={() => setOpen((v) => !v)}>
          {/* Short enough to fit the rail. The full sentence ran past the
              column edge, which is how a rail says "this control does not
              belong here" -- the dialog it opens explains the rest. */}
          {open ? "Cancel" : "Move issue..."}
        </Button>
      </div>
      {open ? (
        <div className="inline-form">
          <Field orientation="horizontal">
            <Field.Label>Target product</Field.Label>
            <Select
              value={targetProductId}
              onValueChange={(next) => pickProduct(next ?? "")}
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
          <Button variant="filled" size="sm"
            disabled={busy || !targetProductId || !targetComponentId}
            onClick={() => void move()}
          >
            Confirm move
          </Button>
          <p className="state-hint small">
            Moving an issue that already has a version or milestone set is rejected by the
            server today (it cannot clear those fields).
          </p>
        </div>
      ) : null}
      {error || targetProductQ.isError ? (
        <p className="field-error">
          {error ?? errorMessage(targetProductQ.error)}
        </p>
      ) : null}
    </div>
  );
}

function GeneralField({
  issue,
  field,
  label,
  value,
}: {
  issue: Issue;
  field: "summary" | "whiteboard" | "opSys" | "platform" | "url";
  label: string;
  value: string;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const [error, setError] = useState<string | null>(null);
  const update = useAppMutation(
    (next: string) => updateIssue({ id: issue.id, changes: { [field]: next } }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy = update.isPending;

  const save = async () => {
    setError(null);
    try {
      await update.mutateAsync(draft);
      // Leaving edit mode is local UI state; cache invalidation owns the
      // refreshed value shown after it closes.
      setEditing(false);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  // Read until asked. Four of these -- whiteboard, OS, platform, URL -- sat
  // open as text inputs in a 352px rail, so the column that should let you
  // GLANCE at an issue's metadata was mostly empty edit boxes for fields almost
  // nobody sets. A value you are not changing is something to read.
  if (!editing) {
    return (
      <div className="field-row-compact">
        <span className="field-label">{label}</span>
        <span className="field-value">{value ? value : <Absent />}</span>
        <Button
          variant="plain"
          size="sm"
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
        <Button variant="gray" size="sm" disabled={busy} onClick={() => void save()}>
          Save
        </Button>
        <Button
          variant="plain"
          size="sm"
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
  issue,
  people,
  product,
  component,
  productDetail,
  products,
  readOnly = false,
}: {
  issue: Issue;
  people: PeopleMap;
  product: IssueProduct;
  component: IssueComponent;
  productDetail: ProductDetail | null;
  products: { id: string; name: string }[];
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
  const updateVersionOrMilestone = useAppMutation(
    (changes: { versionId?: string; milestoneId?: string }) =>
      updateIssue({ id: issue.id, changes }),
    () => invalidatedBy.issueChanged(issue.id),
  );

  const changeVersionOrMilestone = async (changes: {
    versionId?: string;
    milestoneId?: string;
  }) => {
    setVersionMilestoneError(null);
    try {
      await updateVersionOrMilestone.mutateAsync(changes);
    } catch (err) {
      setVersionMilestoneError(errorMessage(err));
    }
  };

  // "Unspecified" is the server's default for opSys and platform, so an issue
  // that has never been touched reads as unset rather than as configured.
  const hasOther = Boolean(
    issue.whiteboard ||
      issue.url ||
      (issue.opSys && issue.opSys !== "Unspecified") ||
      (issue.platform && issue.platform !== "Unspecified"),
  );
  return (
    <div className="fields-panel">
      <RailFieldset disabled={readOnly}>
      {/* The summary is edited in the PAGE HEAD, where the summary is.
          It lived here as "Edit summary" floating at the top of the rail, a
          control several hundred pixels from the words it changes and in a
          column reserved for metadata. A title is not metadata about itself. */}
      {/* Not a permanent text box. The page heading already states the
          summary in full; a second copy in a 22rem rail truncated it and
          invited edits nobody came to make. Editing is a deliberate act. */}
      <StatusControl issue={issue} />
      <RailSection title="Classification" />
      <Classification issue={issue} />
      <AssigneeControl issue={issue} people={people} />

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
          <DescriptionList.Detail>{personName(issue.reporterId, people)}</DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>QA contact</DescriptionList.Term>
          <DescriptionList.Detail>
            {issue.qaContactId ? personName(issue.qaContactId, people) : <Absent />}
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
            value={issue.versionId ?? ""}
            display={
              productDetail.versions.find((v) => v.id === issue.versionId)?.name ?? (
                <Absent />
              )
            }
            options={productDetail.versions.map((v) => ({ value: v.id, label: v.name }))}
            onChange={(next) => {
              void changeVersionOrMilestone({ versionId: next });
            }}
          />
          <RailChoice
            label="Milestone"
            value={issue.milestoneId ?? ""}
            display={
              productDetail.milestones.find((m) => m.id === issue.milestoneId)?.name ?? (
                <Absent />
              )
            }
            options={productDetail.milestones.map((m) => ({ value: m.id, label: m.name }))}
            onChange={(next) => {
              void changeVersionOrMilestone({ milestoneId: next });
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
          issue changes product AND component, so it belongs to the whole group
          rather than to any one line in it. */}
      <MoveControl issue={issue} products={products} />

      {/* Four rows that usually say nothing.
          Whiteboard, OS, platform and URL are unset on most issues, so the rail
          ended with "Whiteboard --", "OS Unspecified", "Platform Unspecified",
          "URL --" -- four lines of absence at the bottom of a column whose job
          is to be glanceable. They appear when they HAVE a value, and behind
          one line when they do not, so setting them is still one click and
          reading an issue that never used them costs nothing. */}
      <RailSection title="Other" />
      {hasOther || showOther ? (
        <>
          <GeneralField issue={issue} field="whiteboard" label="Whiteboard" value={issue.whiteboard ?? ""} />
          <div className="field-row">
            <GeneralField issue={issue} field="opSys" label="OS" value={issue.opSys} />
            <GeneralField issue={issue} field="platform" label="Platform" value={issue.platform} />
          </div>
          <GeneralField issue={issue} field="url" label="URL" value={issue.url ?? ""} />
        </>
      ) : (
        <Button variant="plain" size="sm" onClick={() => setShowOther(true)}>
          Set whiteboard, OS, platform or URL
        </Button>
      )}
      </RailFieldset>
    </div>
  );
}
