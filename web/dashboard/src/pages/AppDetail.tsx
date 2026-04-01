import { useState } from "react";
import { useParams, useNavigate, Link } from "react-router-dom";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { getApp, getAppUsage, deployApp, deleteApp, updatePlan } from "../api";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { Copy, Check, ChevronRight } from "lucide-react";

const PLANS = ["free", "starter", "pro", "enterprise"];

export default function AppDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const [code, setCode] = useState("");
  const [newPlan, setNewPlan] = useState("");
  const [copied, setCopied] = useState(false);

  const { data: app, isLoading, error: appError } = useQuery({
    queryKey: ["app", id],
    queryFn: () => getApp(id!),
    enabled: !!id,
  });

  const { data: usage } = useQuery({
    queryKey: ["app-usage", id],
    queryFn: () => getAppUsage(id!).catch(() => null),
    enabled: !!id,
  });

  // Set newPlan when app loads
  if (app && !newPlan) {
    setNewPlan(app.plan_id);
  }

  const deployMutation = useMutation({
    mutationFn: () => deployApp(id!, code),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["app", id] });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: () => deleteApp(id!),
    onSuccess: () => navigate("/apps"),
  });

  const planMutation = useMutation({
    mutationFn: (planId: string) => updatePlan(id!, planId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["app", id] });
    },
  });

  function handleCopy() {
    if (!app) return;
    navigator.clipboard.writeText(app.api_key).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }

  function handleDelete() {
    if (!id) return;
    const confirmed = window.confirm(`delete app "${id}"? this cannot be undone.`);
    if (!confirmed) return;
    deleteMutation.mutate();
  }

  function handlePlanChange() {
    if (!newPlan || newPlan === app?.plan_id) return;
    planMutation.mutate(newPlan);
  }

  const error = appError?.message ?? "";

  if (isLoading) {
    return <div className="text-[13px] text-muted-foreground py-5">loading...</div>;
  }
  if (error && !app) {
    return (
      <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3">
        {error}
      </div>
    );
  }
  if (!app) {
    return (
      <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3">
        app not found
      </div>
    );
  }

  const totalRequests = usage
    ? Object.values(usage.counters || {}).reduce((a, b) => a + b, 0)
    : 0;

  return (
    <div>
      {/* Breadcrumb */}
      <div className="text-xs text-muted-foreground mb-4 flex items-center gap-1">
        <Link to="/apps" className="text-muted-foreground hover:text-foreground transition-colors">
          apps
        </Link>
        <ChevronRight className="h-3 w-3" />
        <span>{app.id}</span>
      </div>

      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-medium tracking-[0.05em]">// {app.id}</h1>
      </div>

      {error && (
        <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3 mb-4">
          {error}
        </div>
      )}

      {/* Info Panel */}
      <Card className="mb-4">
        <CardHeader>
          <CardTitle>info</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 gap-3">
            <InfoItem label="app id" value={app.id} />
            <InfoItem label="plan" value={app.plan_id} />
            <InfoItem label="version" value={`v${app.version}`} />
            <div className="p-3 bg-background border border-border">
              <div className="text-[10px] uppercase tracking-[0.1em] text-muted-foreground mb-1">
                api key
              </div>
              <div className="text-[13px] text-foreground flex items-center gap-2">
                <span className="flex-1 break-all">{app.api_key}</span>
                <button
                  onClick={handleCopy}
                  className="shrink-0 p-1 border border-border text-muted-foreground hover:border-primary hover:text-primary transition-colors cursor-pointer bg-transparent"
                >
                  {copied ? (
                    <Check className="h-3 w-3" />
                  ) : (
                    <Copy className="h-3 w-3" />
                  )}
                </button>
              </div>
            </div>
            <InfoItem label="created" value={new Date(app.created_at).toLocaleString()} />
            <InfoItem label="updated" value={new Date(app.updated_at).toLocaleString()} />
          </div>
        </CardContent>
      </Card>

      {/* Usage Panel */}
      <Card className="mb-4">
        <CardHeader>
          <CardTitle>usage</CardTitle>
        </CardHeader>
        <CardContent>
          {usage ? (
            <div className="grid grid-cols-[repeat(auto-fit,minmax(140px,1fr))] gap-3">
              <div className="p-3 bg-background border border-border">
                <div className="text-[10px] uppercase tracking-[0.1em] text-muted-foreground mb-1">
                  total requests
                </div>
                <div className="text-[28px] font-bold">{totalRequests}</div>
              </div>
              {Object.entries(usage.counters || {}).map(([key, val]) => (
                <div key={key} className="p-3 bg-background border border-border">
                  <div className="text-[10px] uppercase tracking-[0.1em] text-muted-foreground mb-1">
                    {key}
                  </div>
                  <div className="text-[28px] font-bold">{val}</div>
                </div>
              ))}
            </div>
          ) : (
            <div className="text-[13px] text-muted-foreground">
              no usage data available
            </div>
          )}
        </CardContent>
      </Card>

      {/* Deploy Panel */}
      <Card className="mb-4">
        <CardHeader>
          <CardTitle>deploy</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="mb-4">
            <Label htmlFor="deploy-code">javascript source</Label>
            <Textarea
              id="deploy-code"
              placeholder="// paste your handler code here..."
              value={code}
              onChange={(e) => setCode(e.target.value)}
            />
          </div>
          <Button
            variant="primary"
            onClick={() => deployMutation.mutate()}
            disabled={deployMutation.isPending || !code.trim()}
          >
            {deployMutation.isPending ? "deploying..." : "deploy"}
          </Button>
          {deployMutation.isSuccess && (
            <div className="mt-3 p-2.5 text-xs border border-primary text-primary bg-primary/5">
              deployed version {deployMutation.data.version}
            </div>
          )}
          {deployMutation.isError && (
            <div className="mt-3 p-2.5 text-xs border border-destructive text-destructive bg-destructive/5">
              {deployMutation.error.message}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Danger Zone */}
      <Card className="border-destructive/30">
        <CardHeader>
          <CardTitle className="text-destructive">danger zone</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="mb-4">
            <Label htmlFor="plan-select">change plan</Label>
            <div className="flex gap-2">
              <Select value={newPlan} onValueChange={setNewPlan}>
                <SelectTrigger className="flex-1">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PLANS.map((p) => (
                    <SelectItem key={p} value={p}>
                      {p}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Button
                variant="destructive"
                size="sm"
                onClick={handlePlanChange}
                disabled={planMutation.isPending || newPlan === app.plan_id}
              >
                {planMutation.isPending ? "..." : "update"}
              </Button>
            </div>
          </div>
          {(deleteMutation.isError || planMutation.isError) && (
            <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-2.5 mb-4">
              {deleteMutation.error?.message || planMutation.error?.message}
            </div>
          )}
          <div className="mt-4">
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteMutation.isPending}
            >
              {deleteMutation.isPending ? "deleting..." : "delete app"}
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

function InfoItem({ label, value }: { label: string; value: string }) {
  return (
    <div className="p-3 bg-background border border-border">
      <div className="text-[10px] uppercase tracking-[0.1em] text-muted-foreground mb-1">
        {label}
      </div>
      <div className="text-[13px] text-foreground break-all">{value}</div>
    </div>
  );
}
