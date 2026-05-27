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
import { AlertDialog, Badge, Button, Card, Input } from "@zeroship/ui";
import {
  archiveApp,
  deleteApp,
  unarchiveApp,
  updatePlan,
  type AppRecord,
} from "../../api";

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
                <Card
                  key={plan.id}
                  data-testid={`settings-plan:${plan.id}`}
                  variant={active ? "elevated" : "outline"}
                  interactive={!active}
                  className="grid items-center gap-4 px-5 py-4"
                  style={{ gridTemplateColumns: "1fr auto" }}
                >
                  <div>
                    <div className="font-serif italic font-medium text-[18px] flex items-baseline gap-2">
                      <span>{plan.label}</span>
                      {active && (
                        <Badge
                          data-testid="settings-plan-current"
                          tone="info"
                        >
                          current
                        </Badge>
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
                    <Button
                      onClick={() => upgrade.mutate(plan.id)}
                      loading={upgrade.isPending && upgrade.variables === plan.id}
                      data-testid={`settings-plan-upgrade:${plan.id}`}
                    >
                      Choose {plan.label}
                    </Button>
                  )}
                </Card>
              );
            })}
          </div>
        </Section>

        <Section
          title="Archive"
          helper="Tuck a project away without losing it. Reversible — restore any time from the Archived view on Home."
        >
          <Card
            data-testid="settings-archive"
            className="p-5"
          >
            <div className="font-serif italic font-medium text-[16px] mb-1.5 text-ink">
              {isArchived ? "This project is archived" : "Archive this project"}
            </div>
            <p className="font-serif text-[13.5px] text-ink-soft leading-[1.55] mb-4">
              {isArchived
                ? "It's hidden from the default Home view but everything's intact. Restore to bring it back."
                : "Hide it from the default Home view. Code, data, and deploys stay put. You can restore it later."}
            </p>
            {isArchived ? (
              <Button
                variant="tinted"
                onClick={() => unarchive.mutate()}
                disabled={unarchive.isPending}
                loading={unarchive.isPending}
                data-testid="settings-unarchive"
              >
                Restore project
              </Button>
            ) : (
              <Button
                variant="tinted"
                onClick={() => archive.mutate()}
                disabled={archive.isPending}
                loading={archive.isPending}
                data-testid="settings-archive-btn"
              >
                Archive project
              </Button>
            )}
          </Card>
        </Section>

        <Section
          title={<span className="text-tomato">Danger zone</span>}
          helper="Permanent operations. Take a moment."
          last
          danger
        >
          <Card variant="outline" className="p-5">
            <div className="font-serif italic font-medium text-[16px] mb-1.5 text-ink">
              Delete this project
            </div>
            <p className="font-serif text-[13.5px] text-ink-soft leading-[1.55] mb-4">
              The project, its database, all secrets, deploys, and logs go
              away. This can't be undone.
            </p>
            <Button
              variant="filled"
              intent="destructive"
              onClick={() => setConfirmOpen(true)}
              data-testid="settings-delete"
            >
              Delete project
            </Button>
          </Card>
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
    <AlertDialog
      open={open}
      onOpenChange={(nextOpen) => {
        // While a delete is in flight, do nothing — the AlertDialog
        // must remain open until the mutation resolves. Once it's
        // safe to close, surface the close intent to the parent.
        if (deleting) return;
        if (!nextOpen) onClose();
      }}
    >
      <AlertDialog.Portal>
        <AlertDialog.Backdrop />
        <AlertDialog.Popup data-testid="settings-delete-modal">
          <AlertDialog.Header>
            <AlertDialog.Title>Delete project?</AlertDialog.Title>
            <AlertDialog.Description>
              This permanently deletes the project, database, secrets, and all
              deploys. Type{" "}
              <code className="font-mono text-[12.5px] bg-paper-2 px-1.5 py-0.5 rounded-[2px] border border-rule text-ink">
                {appName || "—"}
              </code>{" "}
              to confirm.
            </AlertDialog.Description>
          </AlertDialog.Header>
          <AlertDialog.Body>
            <Input
              autoFocus
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              placeholder={appName}
              data-testid="settings-delete-typed"
              label="Project name"
            />
          </AlertDialog.Body>
          <AlertDialog.Footer>
            <AlertDialog.Cancel disabled={deleting}>cancel</AlertDialog.Cancel>
            <AlertDialog.Action
              tone="destructive"
              disabled={!matches || deleting}
              loading={deleting}
              preventClose
              onClick={() => {
                if (matches) onConfirm();
              }}
              data-testid="settings-delete-confirm"
            >
              Delete forever
            </AlertDialog.Action>
          </AlertDialog.Footer>
        </AlertDialog.Popup>
      </AlertDialog.Portal>
    </AlertDialog>
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
    <div className="mb-3.5">
      <Input label={label} value={value} readOnly={readOnly} />
    </div>
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
        <Button
          variant="tinted"
          onClick={copy}
          data-testid="settings-copy-id-btn"
        >
          {copied ? "copied" : "copy"}
        </Button>
      </div>
    </div>
  );
}
