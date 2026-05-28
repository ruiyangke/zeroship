// ─── SettingsCanvas — identity, plan, danger zone (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.7) ───
//
// Three sections.
//   • Identity: app name (read-only V1), app id (copyable),
//     URL preview ({slug}.zeroship.app).
//   • Plan: current plan with chip + Free/Maker/Pro upgrade buttons.
//   • Danger zone: typed-confirmation delete. Type the exact app
//     name to enable the red Delete button. On confirm, deleteApp
//     then navigate("/").

import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  archiveApp,
  deleteApp,
  unarchiveApp,
  updatePlan,
  type AppRecord,
} from "../../api";
import { Modal } from "../../components/Modal";
import { StampButton } from "../../components/StampButton";
import { GhostButton } from "../../components/GhostButton";

export interface SettingsCanvasProps {
  appId: string;
  app?: AppRecord;
}

const PLANS: ReadonlyArray<{
  id: string;
  label: string;
  description: string;
}> = [
  { id: "free",   label: "Free",   description: "For poking around. Limited compute." },
  { id: "maker",  label: "Maker",  description: "For real projects. Real CPU budget." },
  { id: "pro",    label: "Pro",    description: "For traffic that grows. Higher quotas." },
];

export function SettingsCanvas({ appId, app }: SettingsCanvasProps) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [confirmOpen, setConfirmOpen] = useState(false);

  const upgrade = useMutation({
    mutationFn: async (plan: string) => updatePlan({ appId, plan_id: plan }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["app", appId] }),
  });
  const del = useMutation({
    mutationFn: async () => deleteApp(appId),
    onSuccess: () => navigate("/", { replace: true }),
  });
  const archive = useMutation({
    mutationFn: async () => archiveApp({ appId }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["apps"] });
      qc.invalidateQueries({ queryKey: ["app", appId] });
      navigate("/home", { replace: true });
    },
  });
  const unarchive = useMutation({
    mutationFn: async () => unarchiveApp({ appId }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["apps"] });
      qc.invalidateQueries({ queryKey: ["app", appId] });
    },
  });

  const currentPlan = app?.plan_id ?? "free";
  const isArchived = app?.archived === true;

  return (
    <div data-testid="settings-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[860px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <Section
          title="Identity"
          helper="The name and address your project lives at."
        >
          <Field label="Project name" value={app?.name ?? "—"} readOnly />
          <CopyField label="Project id" value={appId} />
          <Field
            label="URL"
            value={app ? `${app.name}.zeroship.app` : "—"}
            readOnly
          />
        </Section>

        <Section
          title="Plan"
          helper="What you're paying for. Upgrade when traffic grows."
        >
          <div className="grid gap-3" data-testid="settings-plans">
            {PLANS.map((plan) => {
              const active = plan.id === currentPlan;
              return (
                <div
                  key={plan.id}
                  data-testid={`settings-plan:${plan.id}`}
                  className={
                    "grid items-center gap-4 px-5 py-4 border " +
                    (active
                      ? "bg-paper-2 border-ink"
                      : "bg-white border-rule")
                  }
                  style={{ gridTemplateColumns: "1fr auto" }}
                >
                  <div>
                    <div className="font-serif italic font-medium text-[18px] flex items-baseline gap-2">
                      <span>{plan.label}</span>
                      {active && (
                        <span
                          data-testid="settings-plan-current"
                          className="font-sans not-italic text-[10px] uppercase tracking-[0.18em] text-tomato"
                        >
                          current
                        </span>
                      )}
                    </div>
                    <div className="font-serif text-[13px] text-ink-soft mt-0.5">
                      {plan.description}
                    </div>
                  </div>
                  {active ? (
                    <span className="font-serif italic text-[13px] text-pencil">
                      —
                    </span>
                  ) : (
                    <StampButton
                      onClick={() => upgrade.mutate(plan.id)}
                      loading={upgrade.isPending && upgrade.variables === plan.id}
                      data-testid={`settings-plan-upgrade:${plan.id}`}
                    >
                      Choose {plan.label}
                    </StampButton>
                  )}
                </div>
              );
            })}
          </div>
        </Section>

        <Section
          title="Archive"
          helper="Tuck a project away without losing it. Reversible — restore any time from the Archived view on Home."
        >
          <div
            data-testid="settings-archive"
            className="border border-rule bg-paper-2 p-5 rounded-sm"
          >
            <div className="font-serif italic font-medium text-[16px] mb-1.5 text-ink">
              {isArchived ? "This project is archived" : "Archive this project"}
            </div>
            <p className="font-serif text-[13.5px] text-ink-soft leading-[1.55] mb-4">
              {isArchived
                ? "It's hidden from the default Home view but everything's intact. Restore to bring it back."
                : "Hide it from the default Home view. Code, settings, and deploys stay put. You can restore it later."}
            </p>
            {isArchived ? (
              <GhostButton
                onClick={() => unarchive.mutate()}
                disabled={unarchive.isPending}
                data-testid="settings-unarchive"
              >
                {unarchive.isPending ? "Restoring…" : "Restore project"}
              </GhostButton>
            ) : (
              <GhostButton
                onClick={() => archive.mutate()}
                disabled={archive.isPending}
                data-testid="settings-archive-btn"
              >
                {archive.isPending ? "Archiving…" : "Archive project"}
              </GhostButton>
            )}
          </div>
        </Section>

        <Section
          title={<span className="text-tomato">Danger zone</span>}
          helper="Permanent operations. Take a moment."
          last
          danger
        >
          <div className="border border-tomato/40 bg-tomato/5 p-5 rounded-sm">
            <div className="font-serif italic font-medium text-[16px] mb-1.5 text-ink">
              Delete this project
            </div>
            <p className="font-serif text-[13.5px] text-ink-soft leading-[1.55] mb-4">
              The project, its database, all secrets, deploys, and logs go
              away. This can't be undone.
            </p>
            <GhostButton
              danger
              onClick={() => setConfirmOpen(true)}
              data-testid="settings-delete"
            >
              Delete project
            </GhostButton>
          </div>
        </Section>
      </div>

      <DeleteConfirm
        open={confirmOpen}
        onClose={() => setConfirmOpen(false)}
        appName={app?.name ?? ""}
        onConfirm={() => del.mutate()}
        deleting={del.isPending}
      />
    </div>
  );
}

// ─── delete confirmation: type-the-name gate ────────────────────

function DeleteConfirm({
  open,
  onClose,
  appName,
  onConfirm,
  deleting,
}: {
  open: boolean;
  onClose: () => void;
  appName: string;
  onConfirm: () => void;
  deleting: boolean;
}) {
  const [typed, setTyped] = useState("");
  const matches = appName.length > 0 && typed === appName;

  return (
    <Modal open={open} onClose={() => !deleting && onClose()} title="Delete project?">
      <div data-testid="settings-delete-modal">
        <p className="font-serif text-[14px] text-ink-soft mb-4 leading-[1.55]">
          Type{" "}
          <code className="font-mono text-[12.5px] bg-paper-2 px-1.5 py-0.5 rounded-[2px] border border-rule text-ink">
            {appName || "—"}
          </code>{" "}
          to confirm. The project, database, secrets, and all deploys are
          permanently deleted.
        </p>
        <input
          autoFocus
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          placeholder={appName}
          data-testid="settings-delete-typed"
          className="w-full px-3 py-2 border border-rule bg-white font-mono text-[13px] outline-none focus:border-ink mb-4"
        />
        <div className="flex justify-end gap-3">
          <button
            type="button"
            onClick={onClose}
            disabled={deleting}
            className="font-serif italic text-[14px] text-pencil bg-transparent border-0 cursor-pointer disabled:opacity-50"
          >
            cancel
          </button>
          <button
            type="button"
            onClick={() => matches && onConfirm()}
            disabled={!matches || deleting}
            data-testid="settings-delete-confirm"
            className={
              "font-serif text-[14px] px-4 py-2 border border-tomato text-tomato bg-transparent cursor-pointer hover:bg-tomato hover:text-paper transition-colors disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:bg-transparent disabled:hover:text-tomato"
            }
          >
            {deleting ? "Deleting…" : "Delete forever"}
          </button>
        </div>
      </div>
    </Modal>
  );
}

// ─── reusable section / field primitives ────────────────────────

function Section({
  title,
  helper,
  children,
  last,
  danger,
}: {
  title: React.ReactNode;
  helper?: React.ReactNode;
  children: React.ReactNode;
  last?: boolean;
  danger?: boolean;
}) {
  void danger;
  return (
    <div
      className={"grid gap-12 py-6 " + (last ? "" : "border-b border-rule")}
      style={{ gridTemplateColumns: "260px 1fr" }}
    >
      <div>
        <h3 className="font-serif italic font-medium text-[22px] m-0 mb-1.5">
          {title}
        </h3>
        {helper && (
          <p className="font-serif text-[13.5px] text-ink-soft leading-[1.5]">
            {helper}
          </p>
        )}
      </div>
      <div>{children}</div>
    </div>
  );
}

function Field({
  label,
  value,
  readOnly,
}: {
  label: string;
  value: string;
  readOnly?: boolean;
}) {
  return (
    <label className="block mb-3.5">
      <span className="block label-uc mb-1">{label}</span>
      <input
        value={value}
        readOnly={readOnly}
        className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink read-only:bg-paper-2 read-only:text-ink-soft"
      />
    </label>
  );
}

function CopyField({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    void navigator.clipboard?.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    });
  }
  return (
    <div className="block mb-3.5">
      <span className="block label-uc mb-1">{label}</span>
      <div className="flex gap-2">
        <input
          value={value}
          readOnly
          className="flex-1 px-3 py-2.5 border border-rule bg-paper-2 font-mono text-[12.5px] text-ink-soft outline-none"
          data-testid="settings-copy-id"
        />
        <button
          type="button"
          onClick={copy}
          className="px-3 py-2.5 border border-rule bg-white font-serif italic text-[13px] text-ink-soft hover:border-ink hover:text-ink cursor-pointer"
          data-testid="settings-copy-id-btn"
        >
          {copied ? "copied" : "copy"}
        </button>
      </div>
    </div>
  );
}
