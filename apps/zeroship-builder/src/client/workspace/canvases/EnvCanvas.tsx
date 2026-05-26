// ─── EnvCanvas — variables + secrets (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.6) ────────────────
//
// Two sections in one canvas. Variables render values inline; secrets
// only render the key (the control plane never exposes the value, so
// the form is set-only — there is no "edit" path, only "rotate by
// re-setting"). Both use the existing apps.* RPCs.
//
// Note on the wire: set/delete procedures use one object input so the
// generated RPC stubs can forward every field over the single-input
// `/_zs/v1/<id>` contract.

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  listVars,
  setVar,
  deleteVar,
  listSecrets,
  setSecret,
  deleteSecret,
} from "../../api";
import { StampButton } from "../../components/StampButton";

export interface EnvCanvasProps {
  appId: string;
}

export function EnvCanvas({ appId }: EnvCanvasProps) {
  return (
    <div data-testid="env-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[860px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <Variables appId={appId} />
        <div className="h-12" />
        <Secrets appId={appId} />
      </div>
    </div>
  );
}

// ─── variables (key + value, both visible) ───────────────────────

function Variables({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data: vars, isLoading } = useQuery({
    queryKey: ["env-vars", appId],
    queryFn: () => listVars(appId),
    retry: false,
  });
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  const add = useMutation({
    mutationFn: async () => setVar({ appId, key: newKey, value: newVal }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["env-vars", appId] });
      setAdding(false);
      setNewKey("");
      setNewVal("");
    },
  });
  const del = useMutation({
    mutationFn: async (key: string) => deleteVar({ appId, key }),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["env-vars", appId] }),
  });

  const items = vars?.vars ?? [];

  return (
    <section data-testid="env-variables-section">
      <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
        Variables
      </h2>
      <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
        Settings that are fine to share — URLs, feature flags, defaults.
        Anyone with your code can see these.
      </p>

      <div>
        {isLoading && (
          <div className="font-serif italic text-pencil py-2">loading…</div>
        )}
        {!isLoading && items.length === 0 && !adding && (
          <div
            data-testid="env-variables-empty"
            className="font-serif italic text-pencil py-3"
          >
            Nothing here yet — variables are the public knobs your app reads at runtime.
          </div>
        )}
        {items.map((v) => (
          <Row
            key={v.key}
            mono
            name={v.key}
            value={v.value}
            onDelete={() => del.mutate(v.key)}
          />
        ))}

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
            type="text"
            testidPrefix="env-add-var"
          />
        ) : (
          <button
            type="button"
            onClick={() => setAdding(true)}
            data-testid="env-add-var"
            className="mt-3 font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
          >
            + Add a variable
          </button>
        )}
      </div>
    </section>
  );
}

// ─── secrets (key only — values are server-only) ────────────────

function Secrets({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data: secrets, isLoading } = useQuery({
    queryKey: ["env-secrets", appId],
    queryFn: () => listSecrets(appId),
    retry: false,
  });
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  const add = useMutation({
    mutationFn: async () => setSecret({ appId, key: newKey, value: newVal }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["env-secrets", appId] });
      setAdding(false);
      setNewKey("");
      setNewVal("");
    },
  });
  const del = useMutation({
    mutationFn: async (key: string) => deleteSecret({ appId, key }),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["env-secrets", appId] }),
  });

  const items = secrets?.secrets ?? [];

  return (
    <section data-testid="env-secrets-section">
      <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
        <em className="italic">Secrets</em>
      </h2>
      <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
        Things you don't want public — API keys, passwords, tokens.
        We encrypt these. They're never shown again after you set them.
      </p>

      <div>
        {isLoading && (
          <div className="font-serif italic text-pencil py-2">loading…</div>
        )}
        {!isLoading && items.length === 0 && !adding && (
          <div
            data-testid="env-secrets-empty"
            className="font-serif italic text-pencil py-3"
          >
            Nothing here yet — secrets are the keys you'd never paste in chat.
          </div>
        )}
        {items.map((k) => (
          <Row
            key={k}
            secret
            name={k}
            value="·····················"
            onDelete={() => del.mutate(k)}
            actionLabel="rotate"
          />
        ))}

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
            valuePlaceholder="secret value"
            keyPlaceholder="SECRET_NAME"
            type="password"
            testidPrefix="env-add-secret"
          />
        ) : (
          <button
            type="button"
            onClick={() => setAdding(true)}
            data-testid="env-add-secret"
            className="mt-3 font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80 focus:outline-2 focus:outline-tomato focus:outline-offset-2"
          >
            + Add a secret
          </button>
        )}
      </div>
    </section>
  );
}

// ─── shared row primitives ───────────────────────────────────────

function Row({
  name,
  value,
  secret,
  onDelete,
  actionLabel = "delete",
}: {
  name: string;
  value: string;
  secret?: boolean;
  mono?: boolean;
  onDelete: () => void;
  actionLabel?: string;
}) {
  return (
    <div
      // Phone: name on top, value below, action right-aligned in
      // a third row. Tablet+: original 200px/1fr/60px layout returns.
      className="grid items-center gap-2 sm:gap-4 py-3 border-b border-rule-2 grid-cols-[1fr_auto] sm:grid-cols-[200px_1fr_60px]"
    >
      <span className="font-mono text-[13px] text-ink truncate">{name}</span>
      <span
        className={
          "text-[13px] truncate col-span-2 sm:col-auto sm:order-none order-3 " +
          (secret
            ? "font-mono text-pencil tracking-[0.2em]"
            : "font-mono text-ink-soft")
        }
      >
        {value}
      </span>
      <span className="text-right">
        <button
          type="button"
          onClick={onDelete}
          aria-label={`${actionLabel} ${name}`}
          className="font-serif italic text-[12.5px] text-ink-soft hover:text-tomato bg-transparent border-0 cursor-pointer focus:outline-2 focus:outline-tomato focus:outline-offset-2 px-1"
        >
          {actionLabel}
        </button>
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
  type,
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
  type: "text" | "password";
  testidPrefix: string;
}) {
  const canSave = keyValue.length > 0 && valValue.length > 0;
  return (
    <div
      className="grid items-center gap-4 py-3 border-b border-rule-2"
      style={{ gridTemplateColumns: "200px 1fr 120px" }}
    >
      <input
        autoFocus
        placeholder={keyPlaceholder}
        value={keyValue}
        onChange={(e) => onKeyChange(e.target.value)}
        data-testid={`${testidPrefix}-key`}
        className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
      />
      <input
        type={type}
        placeholder={valuePlaceholder}
        value={valValue}
        onChange={(e) => onValChange(e.target.value)}
        data-testid={`${testidPrefix}-value`}
        className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
      />
      <div className="flex gap-2 justify-end">
        <button
          type="button"
          onClick={onCancel}
          className="font-serif italic text-[12px] text-pencil bg-transparent border-0 cursor-pointer"
        >
          cancel
        </button>
        <StampButton
          onClick={() => canSave && onSave()}
          disabled={!canSave}
          loading={saving}
        >
          Save
        </StampButton>
      </div>
    </div>
  );
}
