// ─── EnvTab — key cabinet (vars + secrets) ──────────────────────

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { listVars, setVar, deleteVar, listSecrets, setSecret, deleteSecret } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { TabDrawer } from "../components/TabDrawer";
import { GhostButton } from "../../components/GhostButton";
import { StampButton } from "../../components/StampButton";

export function EnvTab() {
  const { appId } = useWorkspace();

  return (
    <div className="h-full flex flex-col" data-testid="env-tab">
      <div className="flex-1 px-12 pt-10 pb-0 overflow-auto max-w-[860px] w-full mx-auto">
        <Variables appId={appId} />
        <div className="h-10" />
        <Secrets appId={appId} />
      </div>
      <TabDrawer appId={appId} />
    </div>
  );
}

function Variables({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data: vars } = useQuery({
    queryKey: ["vars", appId],
    queryFn: () => listVars(appId),
  });
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  const add = useMutation({
    mutationFn: () => setVar(appId, newKey, newVal),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["vars", appId] });
      setAdding(false); setNewKey(""); setNewVal("");
    },
  });
  const del = useMutation({
    mutationFn: (key: string) => deleteVar(appId, key),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["vars", appId] }),
  });

  return (
    <section>
      <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">Variables</h2>
      <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
        Settings that are fine to share — URLs, feature flags, defaults. Anyone with your code can see these.
      </p>
      <div>
        {(vars?.vars ?? []).map((v) => (
          <Row key={v.key} mono name={v.key} value={v.value} onDelete={() => del.mutate(v.key)} />
        ))}
        {(vars?.vars ?? []).length === 0 && !adding && (
          <div className="font-serif italic text-pencil py-2">No variables yet.</div>
        )}
        {adding ? (
          <div className="grid items-center gap-4 py-3 border-b border-rule-2" style={{ gridTemplateColumns: "200px 1fr 80px" }}>
            <input
              autoFocus
              placeholder="VAR_NAME"
              value={newKey}
              onChange={(e) => setNewKey(e.target.value.toUpperCase())}
              className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
            />
            <input
              placeholder="value"
              value={newVal}
              onChange={(e) => setNewVal(e.target.value)}
              className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
            />
            <div className="flex gap-2">
              <button
                type="button"
                onClick={() => setAdding(false)}
                className="font-serif italic text-[12px] text-pencil bg-transparent border-0 cursor-pointer"
              >
                cancel
              </button>
              <StampButton onClick={() => newKey && newVal && add.mutate()} disabled={!newKey || !newVal} loading={add.isPending}>
                Save
              </StampButton>
            </div>
          </div>
        ) : (
          <button
            type="button"
            onClick={() => setAdding(true)}
            className="mt-3 font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80"
          >
            + Add a variable
          </button>
        )}
      </div>
    </section>
  );
}

function Secrets({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data: secrets } = useQuery({
    queryKey: ["secrets", appId],
    queryFn: () => listSecrets(appId),
  });
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  const add = useMutation({
    mutationFn: () => setSecret(appId, newKey, newVal),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["secrets", appId] });
      setAdding(false); setNewKey(""); setNewVal("");
    },
  });
  const del = useMutation({
    mutationFn: (key: string) => deleteSecret(appId, key),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["secrets", appId] }),
  });

  return (
    <section>
      <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
        <em className="italic">Secrets</em> · 🔒
      </h2>
      <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
        Things you don't want public — API keys, passwords, tokens. We encrypt these. They're never shown again after you set them.
      </p>
      <div>
        {(secrets?.secrets ?? []).map((k) => (
          <Row key={k} secret name={k} value={"·····················"} onDelete={() => del.mutate(k)} actionLabel="rotate" />
        ))}
        {(secrets?.secrets ?? []).length === 0 && !adding && (
          <div className="font-serif italic text-pencil py-2">No secrets yet.</div>
        )}
        {adding ? (
          <div className="grid items-center gap-4 py-3 border-b border-rule-2" style={{ gridTemplateColumns: "200px 1fr 80px" }}>
            <input
              autoFocus
              placeholder="SECRET_NAME"
              value={newKey}
              onChange={(e) => setNewKey(e.target.value.toUpperCase())}
              className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
            />
            <input
              type="password"
              placeholder="secret value"
              value={newVal}
              onChange={(e) => setNewVal(e.target.value)}
              className="font-mono text-[13px] px-2 py-1.5 bg-white border border-rule outline-none focus:border-ink"
            />
            <div className="flex gap-2">
              <button type="button" onClick={() => setAdding(false)} className="font-serif italic text-[12px] text-pencil bg-transparent border-0 cursor-pointer">
                cancel
              </button>
              <StampButton onClick={() => newKey && newVal && add.mutate()} disabled={!newKey || !newVal} loading={add.isPending}>
                Save
              </StampButton>
            </div>
          </div>
        ) : (
          <button
            type="button"
            onClick={() => setAdding(true)}
            className="mt-3 font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80"
          >
            + Add a secret
          </button>
        )}
      </div>
    </section>
  );
}

function Row({ name, value, secret, mono, onDelete, actionLabel = "delete" }: {
  name: string; value: string;
  secret?: boolean; mono?: boolean;
  onDelete: () => void;
  actionLabel?: string;
}) {
  return (
    <div className="grid items-center gap-4 py-3 border-b border-rule-2" style={{ gridTemplateColumns: "200px 1fr 90px 60px" }}>
      <span className={"font-mono text-[13px] text-ink"}>{name}</span>
      <span className={"text-[13px] " + (secret ? "font-mono text-pencil tracking-[0.2em]" : "font-mono text-ink-soft")}>
        {value}
      </span>
      <span className="font-sans text-[10px] uppercase tracking-[0.16em] text-pencil">upd. now</span>
      <span className="text-right">
        <button
          type="button"
          onClick={onDelete}
          className="font-serif italic text-[12.5px] text-ink-soft hover:text-tomato bg-transparent border-0 cursor-pointer"
        >
          {actionLabel}
        </button>
      </span>
    </div>
  );
}
