// ─── SettingsTab — plan + delete ────────────────────────────────
// Simple form to switch plan + a danger zone for delete. Uses the
// existing /api/apps/:id endpoints; everything else (custom domain
// etc.) is stubbed with "coming soon" copy until backends land.

import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { deleteApp, updatePlan } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const PLANS = ["free", "pro"];

export function SettingsTab() {
  const { appId, app } = useWorkspace();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const [plan, setPlan] = useState(app?.plan_id ?? "free");
  const [confirmName, setConfirmName] = useState("");

  const planMut = useMutation({
    mutationFn: (newPlan: string) => updatePlan(appId, newPlan),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["app", appId] }),
  });

  const deleteMut = useMutation({
    mutationFn: () => deleteApp(appId),
    onSuccess: () => navigate("/", { replace: true }),
  });

  const canDelete = !!app && confirmName === app.name;

  return (
    <div
      data-testid="settings-tab"
      className="h-full overflow-auto p-6 max-w-2xl mx-auto space-y-6"
    >
      <header>
        <h1 className="text-lg font-medium tracking-wider">// settings</h1>
        <p className="text-xs text-muted-foreground mt-1">
          configure plan, domain, and lifecycle for{" "}
          <span className="font-mono text-foreground">{app?.name ?? "—"}</span>
        </p>
      </header>

      <Card>
        <CardHeader><CardTitle>plan</CardTitle></CardHeader>
        <CardContent className="space-y-3">
          <div className="grid grid-cols-[1fr_auto] gap-2 items-end">
            <div>
              <Label htmlFor="plan">tier</Label>
              <Select value={plan} onValueChange={setPlan}>
                <SelectTrigger id="plan"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {PLANS.map((p) => (
                    <SelectItem key={p} value={p}>{p}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <Button
              variant="primary"
              disabled={planMut.isPending || plan === app?.plan_id}
              onClick={() => planMut.mutate(plan)}
            >
              {planMut.isPending ? "saving…" : "save"}
            </Button>
          </div>
          {planMut.isError && (
            <div className="text-xs text-destructive">{planMut.error.message}</div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>custom domain</CardTitle></CardHeader>
        <CardContent>
          <div className="text-xs text-muted-foreground">
            coming soon — for now your app lives at{" "}
            <code className="font-mono text-foreground">/apps/{app?.name}</code>{" "}
            on the gateway.
          </div>
        </CardContent>
      </Card>

      <Card className="border-destructive/40">
        <CardHeader>
          <CardTitle className="text-destructive">danger zone</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="text-xs text-muted-foreground">
            permanently delete this app, its bundle, env vars, and assets.
            type the project name to confirm.
          </div>
          <div className="grid grid-cols-[1fr_auto] gap-2 items-end">
            <Input
              placeholder={app?.name ?? ""}
              value={confirmName}
              onChange={(e) => setConfirmName(e.target.value)}
              data-testid="settings-delete-confirm"
            />
            <Button
              variant="destructive"
              disabled={!canDelete || deleteMut.isPending}
              onClick={() => deleteMut.mutate()}
              data-testid="settings-delete-button"
            >
              {deleteMut.isPending ? "deleting…" : "delete"}
            </Button>
          </div>
          {deleteMut.isError && (
            <div className="text-xs text-destructive">{deleteMut.error.message}</div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
