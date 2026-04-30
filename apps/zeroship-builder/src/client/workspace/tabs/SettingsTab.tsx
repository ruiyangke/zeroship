// ─── SettingsTab — project details, domain, plan, danger zone ──

import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { deleteApp, updatePlan } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { TabDrawer } from "../components/TabDrawer";
import { GhostButton } from "../../components/GhostButton";
import { StampButton } from "../../components/StampButton";

export function SettingsTab() {
  const { appId, app } = useWorkspace();
  const navigate = useNavigate();
  const qc = useQueryClient();

  const [name, setName] = useState(app?.name ?? "");
  const [tagline, setTagline] = useState("");
  const [customDomain, setCustomDomain] = useState("");

  const upgrade = useMutation({
    mutationFn: (plan: string) => updatePlan(appId, plan),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["app", appId] }),
  });
  const del = useMutation({
    mutationFn: () => deleteApp(appId),
    onSuccess: () => navigate("/", { replace: true }),
  });

  return (
    <div className="h-full flex flex-col" data-testid="settings-tab">
      <div className="flex-1 px-12 pt-10 pb-0 overflow-auto max-w-[860px] w-full mx-auto">
        <Section title="General" helper="The name and tagline shown on your project's page and in the studio.">
          <Field label="Project name" value={name} onChange={setName} />
          <Field label="Tagline" value={tagline} onChange={setTagline} placeholder="A small space for your supper club." />
        </Section>

        <Section title="Domain" helper="Use the studio default, or bring your own. We'll handle TLS." id="domain">
          <Field label="Studio URL" value={`${app?.name ?? "—"}.zeroship.app`} readOnly />
          <Field label="Custom domain" value={customDomain} onChange={setCustomDomain} placeholder="e.g. recipes.suppersociety.com" />
          <a href="#" className="font-serif italic text-tomato text-[14px]" style={{ textDecoration: "none" }}>
            Set up a custom domain →
          </a>
        </Section>

        <Section title="Plan" helper="Currently free. Upgrade when you need more compute or your traffic grows.">
          <div className="grid items-center gap-6 bg-paper-2 border border-rule px-7 py-5" style={{ gridTemplateColumns: "1fr auto" }}>
            <div>
              <div className="font-serif italic text-[20px]">
                <em className="italic text-tomato">{app?.plan_id ?? "free"}</em>
              </div>
              <div className="font-mono text-[12px] text-ink-soft mt-1">CPU 320ms / 50ms · 6.4× over today</div>
              <div className="h-1.5 bg-paper-3 rounded-full overflow-hidden mt-1.5">
                <div className="bg-tomato h-full" style={{ width: "64%" }} />
              </div>
            </div>
            <StampButton onClick={() => upgrade.mutate("pro")} loading={upgrade.isPending}>
              Upgrade
            </StampButton>
          </div>
        </Section>

        <Section title={<span className="text-tomato">Danger zone</span>} helper="Permanent operations. Take a moment." last>
          <div className="flex flex-col items-start gap-3">
            <GhostButton>Transfer ownership</GhostButton>
            <GhostButton danger onClick={() => {
              if (confirm("Delete this project? This is permanent.")) del.mutate();
            }} disabled={del.isPending}>
              {del.isPending ? "Deleting…" : "Delete project"}
            </GhostButton>
          </div>
        </Section>
      </div>
      <TabDrawer appId={appId} />
    </div>
  );
}

function Section({ title, helper, children, last, id }: {
  title: React.ReactNode; helper: React.ReactNode;
  children: React.ReactNode; last?: boolean; id?: string;
}) {
  return (
    <div id={id} className={"grid gap-12 py-6 " + (last ? "" : "border-b border-rule")} style={{ gridTemplateColumns: "280px 1fr" }}>
      <div>
        <h3 className="font-serif italic font-medium text-[22px] m-0 mb-1.5">{title}</h3>
        {helper && <p className="font-serif text-[13.5px] text-ink-soft leading-[1.5]">{helper}</p>}
      </div>
      <div>{children}</div>
    </div>
  );
}

function Field({
  label, value, onChange, placeholder, readOnly,
}: {
  label: string; value: string; onChange?: (v: string) => void; placeholder?: string; readOnly?: boolean;
}) {
  return (
    <label className="block mb-3.5">
      <span className="block label-uc mb-1">{label}</span>
      <input
        value={value}
        onChange={(e) => onChange?.(e.target.value)}
        placeholder={placeholder}
        readOnly={readOnly}
        className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[15px] text-ink outline-none focus:border-ink read-only:bg-paper-2 read-only:text-ink-soft"
      />
    </label>
  );
}
