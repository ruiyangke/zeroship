// ─── EnvCanvas — the project `.env` (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.6) ────────────────────
//
// The console is a PURE creator app: a preview sandbox has no secret
// vault, so the old vars+secrets split collapses to ONE `.env` file in
// the sandbox project root. This canvas reads/writes it via getEnv /
// setEnv / deleteEnv (server-side read-modify-write of `.env` over the
// sandbox files API). Vite auto-restarts the dev server on `.env`
// change, so edits apply on the next preview reload.
//
// Note on the wire: set/delete procedures use one object input so the
// generated RPC stubs can forward every field over the single-input
// `/__zeroship/v1/<id>` contract.
//
// Presentation is crystal (@zeroship/ui): the section frame is a
// FormSection, the add-row uses Field + Input + Button, and the variable
// rows + status copy live in the co-located EnvCanvas.css over --zs-*
// tokens.

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, FormSection, Input } from "@zeroship/ui";
import { getEnv, setEnv, deleteEnv } from "../../api";
import "./EnvCanvas.css";

export interface EnvCanvasProps {
  appId: string;
}

export function EnvCanvas({ appId }: EnvCanvasProps) {
  return (
    <div data-testid="env-canvas" className="env-canvas">
      <div className="env-canvas__page">
        <Environment appId={appId} />
      </div>
    </div>
  );
}

// ─── environment (key + value lines from the project `.env`) ──────

function Environment({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data: env, isLoading } = useQuery({
    queryKey: ["env", appId],
    queryFn: () => getEnv(appId),
    retry: false,
  });
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  const add = useMutation({
    mutationFn: async () => setEnv({ appId, key: newKey, value: newVal }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["env", appId] });
      setAdding(false);
      setNewKey("");
      setNewVal("");
    },
  });
  const del = useMutation({
    mutationFn: async (key: string) => deleteEnv({ appId, key }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["env", appId] }),
  });

  const items = env?.vars ?? [];

  return (
    <FormSection
      data-testid="env-variables-section"
      title="Environment"
      description={
        <>
          Keys your app reads at runtime — URLs, feature flags, API keys.
          These live in a <code className="env-canvas__code">.env</code> file in
          your project. Saving one restarts the dev server so the next preview
          reload picks it up.
        </>
      }
    >
      <div>
        {isLoading && <div className="env-status">loading…</div>}
        {!isLoading && items.length === 0 && !adding && (
          <div data-testid="env-variables-empty" className="env-status">
            Nothing here yet — add the keys your app reads at runtime.
          </div>
        )}

        <div className="env-rows">
          {items.map((v) => (
            <Row
              key={v.key}
              name={v.key}
              value={v.value}
              onDelete={() => del.mutate(v.key)}
            />
          ))}
        </div>

        {adding ? (
          <AddRow
            keyValue={newKey}
            valValue={newVal}
            onKeyChange={(s) => setNewKey(s.toUpperCase())}
            onValChange={setNewVal}
            onCancel={() => {
              setAdding(false);
              setNewKey("");
              setNewVal("");
            }}
            onSave={() => add.mutate()}
            saving={add.isPending}
            valuePlaceholder="value"
            keyPlaceholder="VAR_NAME"
            testidPrefix="env-add-var"
          />
        ) : (
          <Button
            variant="plain"
            size="small"
            onClick={() => setAdding(true)}
            data-testid="env-add-var"
            className="env-add-trigger"
          >
            + Add a variable
          </Button>
        )}
      </div>
    </FormSection>
  );
}

// ─── shared row primitives ───────────────────────────────────────

function Row({
  name,
  value,
  onDelete,
  actionLabel = "delete",
}: {
  name: string;
  value: string;
  onDelete: () => void;
  actionLabel?: string;
}) {
  return (
    // Phone: name on top, value below, action right-aligned in a third
    // row. Tablet+: original 200px/1fr/auto layout returns (EnvCanvas.css).
    <div className="env-row">
      <span className="env-row__key">{name}</span>
      <span className="env-row__value">{value}</span>
      <span className="env-row__action">
        <Button
          variant="plain"
          intent="destructive"
          size="small"
          onClick={onDelete}
          aria-label={`${actionLabel} ${name}`}
        >
          {actionLabel}
        </Button>
      </span>
    </div>
  );
}

function AddRow({
  keyValue,
  valValue,
  onKeyChange,
  onValChange,
  onCancel,
  onSave,
  saving,
  keyPlaceholder,
  valuePlaceholder,
  testidPrefix,
}: {
  keyValue: string;
  valValue: string;
  onKeyChange: (s: string) => void;
  onValChange: (s: string) => void;
  onCancel: () => void;
  onSave: () => void;
  saving: boolean;
  keyPlaceholder: string;
  valuePlaceholder: string;
  testidPrefix: string;
}) {
  const canSave = keyValue.length > 0 && valValue.length > 0;
  return (
    <div className="env-add">
      <Input
        autoFocus
        aria-label="Variable name"
        placeholder={keyPlaceholder}
        value={keyValue}
        onChange={(e) => onKeyChange(e.target.value)}
        data-testid={`${testidPrefix}-key`}
        className="env-add__input"
      />
      <Input
        type="text"
        aria-label="Variable value"
        placeholder={valuePlaceholder}
        value={valValue}
        onChange={(e) => onValChange(e.target.value)}
        data-testid={`${testidPrefix}-value`}
        className="env-add__input"
      />
      <div className="env-add__actions">
        <Button variant="plain" size="small" onClick={onCancel}>
          cancel
        </Button>
        <Button
          size="small"
          onClick={() => canSave && onSave()}
          disabled={!canSave}
          loading={saving}
        >
          Save
        </Button>
      </div>
    </div>
  );
}
