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

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getEnv, setEnv, deleteEnv } from "../../api";
import { StampButton } from "../../components/StampButton";

export interface EnvCanvasProps {
  appId: string;
}

export function EnvCanvas({ appId }: EnvCanvasProps) {
  return (
    <div data-testid="env-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[860px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
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
    <section data-testid="env-variables-section">
      <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
        Environment
      </h2>
      <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
        Keys your app reads at runtime — URLs, feature flags, API keys.
        These live in a <code className="font-mono text-[12.5px]">.env</code>{" "}
        file in your project. Saving one restarts the dev server so the
        next preview reload picks it up.
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
            Nothing here yet — add the keys your app reads at runtime.
          </div>
        )}
        {items.map((v) => (
          <Row
            key={v.key}
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
    <div
      // Phone: name on top, value below, action right-aligned in
      // a third row. Tablet+: original 200px/1fr/60px layout returns.
      className="grid items-center gap-2 sm:gap-4 py-3 border-b border-rule-2 grid-cols-[1fr_auto] sm:grid-cols-[200px_1fr_60px]"
    >
      <span className="font-mono text-[13px] text-ink truncate">{name}</span>
      <span className="text-[13px] truncate col-span-2 sm:col-auto sm:order-none order-3 font-mono text-ink-soft">
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
        type="text"
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
