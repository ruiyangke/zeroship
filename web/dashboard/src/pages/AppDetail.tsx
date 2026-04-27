import { useState, useEffect } from "react";
import { useParams, useNavigate, Link } from "react-router-dom";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { getApp, getAppUsage, getAppLogs, deployApp, deleteApp, updatePlan, callRpc, createApp } from "../api";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { Input } from "@/components/ui/input";
import { Copy, Check, ChevronRight, CopyPlus } from "lucide-react";
import Editor from '@monaco-editor/react';

const PLANS = ["free", "pro"];

export default function AppDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const [code, setCode] = useState("");
  const [codePrefilled, setCodePrefilled] = useState(false);
  const [newPlan, setNewPlan] = useState("");
  const [copied, setCopied] = useState(false);
  const [showApiKey, setShowApiKey] = useState(false);
  const [curlCopied, setCurlCopied] = useState(false);
  const [rpcMethod, setRpcMethod] = useState("");
  const [rpcParams, setRpcParams] = useState("[]");
  const [rpcResult, setRpcResult] = useState<string | null>(null);
  const [rpcRunning, setRpcRunning] = useState(false);
  const [rpcError, setRpcError] = useState<string | null>(null);
  const [duplicating, setDuplicating] = useState(false);

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

  const { data: logs } = useQuery({
    queryKey: ["app-logs", id],
    queryFn: () => getAppLogs(id!).catch(() => [] as string[]),
    enabled: !!id,
    refetchInterval: 5000,
  });

  // Set newPlan when app loads
  useEffect(() => {
    if (app && !newPlan) {
      setNewPlan(app.plan_id);
    }
  }, [app]);

  // Pre-fill deploy textarea with current server_js
  useEffect(() => {
    if (app && app.server_js && !codePrefilled) {
      setCode(app.server_js);
      setCodePrefilled(true);
    }
  }, [app]);

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

  async function handleRpc() {
    if (!id || !rpcMethod.trim()) return;
    setRpcRunning(true);
    setRpcResult(null);
    setRpcError(null);
    try {
      const params = JSON.parse(rpcParams);
      const result = await callRpc(id, rpcMethod, params);
      const resultObj = result as any;
      if (resultObj && resultObj.error) {
        setRpcError(JSON.stringify(resultObj.error, null, 2));
      } else {
        setRpcResult(JSON.stringify(result, null, 2));
      }
    } catch (e: any) {
      setRpcError(e.message || "RPC call failed");
    } finally {
      setRpcRunning(false);
    }
  }

  async function handleDuplicate() {
    if (!app || !id) return;
    setDuplicating(true);
    try {
      const newId = `${id}-copy`;
      await createApp(newId, app.plan_id);
      const currentCode = app.server_js || code;
      if (currentCode) {
        await deployApp(newId, currentCode);
      }
      queryClient.invalidateQueries({ queryKey: ["apps"] });
      navigate(`/apps/${newId}`);
    } catch (e: any) {
      alert(`Failed to duplicate: ${e.message}`);
    } finally {
      setDuplicating(false);
    }
  }

  const error = appError?.message ?? "";
  const isNotFound = error.includes("404") || error.toLowerCase().includes("not found");

  if (isLoading) {
    return <div className="text-[13px] text-muted-foreground py-5">loading...</div>;
  }
  if (error && !app) {
    return (
      <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3">
        <div className="font-medium mb-1">
          {isNotFound ? "app not found" : "error loading app"}
        </div>
        <div className="text-muted-foreground">{error}</div>
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

  const totalRequests = usage?.requests ?? 0;

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
        <Button
          variant="outline"
          size="sm"
          onClick={handleDuplicate}
          disabled={duplicating}
          className="flex items-center gap-1.5"
        >
          <CopyPlus className="h-3.5 w-3.5" />
          {duplicating ? "duplicating..." : "duplicate"}
        </Button>
      </div>

      {/* Quick Start */}
      <Card className="mb-4 border-primary/30">
        <CardHeader>
          <CardTitle>quick start</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="relative">
            <pre className="p-3 bg-background border border-border text-xs font-mono whitespace-pre-wrap overflow-auto">
{`curl -X POST http://localhost:3333/rpc \\
  -H 'X-App-Id: ${app.id}' \\
  -H 'Content-Type: application/json' \\
  -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'`}
            </pre>
            <button
              onClick={() => {
                const cmd = `curl -X POST http://localhost:3333/rpc -H 'X-App-Id: ${app.id}' -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'`;
                navigator.clipboard.writeText(cmd).then(() => {
                  setCurlCopied(true);
                  setTimeout(() => setCurlCopied(false), 2000);
                });
              }}
              className="absolute top-2 right-2 p-1.5 border border-border text-muted-foreground hover:border-primary hover:text-primary transition-colors cursor-pointer bg-card"
            >
              {curlCopied ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
            </button>
          </div>
        </CardContent>
      </Card>

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
            <InfoItem
              label="deploy"
              value={app.deploy_hash ? `${app.deploy_hash.slice(0, 12)}…` : "—"}
            />
            <div className="p-3 bg-background border border-border">
              <div className="text-[10px] uppercase tracking-[0.1em] text-muted-foreground mb-1">
                api key
              </div>
              <div className="text-[13px] text-foreground flex items-center gap-2">
                <span className="flex-1 break-all font-mono">
                  {showApiKey
                    ? (app.api_key ?? "")
                    : app.api_key
                      ? `${"*".repeat(Math.max(0, app.api_key.length - 8))}${app.api_key.slice(-8)}`
                      : "—"}
                </span>
                <button
                  onClick={() => setShowApiKey((v) => !v)}
                  className="shrink-0 px-1.5 py-0.5 text-[10px] border border-border text-muted-foreground hover:border-primary hover:text-primary transition-colors cursor-pointer bg-transparent"
                >
                  {showApiKey ? "hide" : "show"}
                </button>
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
              {Object.entries(usage || {}).map(([key, val]) => (
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
            <div className="mt-1.5 border border-border overflow-hidden">
              <Editor
                height="300px"
                defaultLanguage="javascript"
                theme="vs-dark"
                value={code}
                onChange={(v) => setCode(v ?? "")}
                options={{
                  minimap: { enabled: false },
                  fontSize: 13,
                  fontFamily: "'JetBrains Mono', monospace",
                  lineNumbers: "on",
                  scrollBeyondLastLine: false,
                  automaticLayout: true,
                  tabSize: 2,
                }}
              />
            </div>
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
              deployed: {deployMutation.data.deploy_hash.slice(0, 12)}…
            </div>
          )}
          {deployMutation.isError && (
            <div className="mt-3 p-2.5 text-xs border-2 border-destructive text-destructive bg-destructive/5">
              <div className="font-medium mb-1">deploy failed</div>
              <div>{deployMutation.error.message}</div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* Current Code */}
      {app.server_js && (
        <Card className="mb-4">
          <CardHeader>
            <CardTitle>current code</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="border border-border overflow-hidden">
              <Editor
                height="300px"
                defaultLanguage="javascript"
                theme="vs-dark"
                value={app.server_js}
                options={{
                  readOnly: true,
                  minimap: { enabled: false },
                  fontSize: 13,
                  fontFamily: "'JetBrains Mono', monospace",
                  lineNumbers: "on",
                  scrollBeyondLastLine: false,
                  automaticLayout: true,
                  tabSize: 2,
                  domReadOnly: true,
                }}
              />
            </div>
          </CardContent>
        </Card>
      )}

      {/* Test */}
      <Card className="mb-4">
        <CardHeader>
          <CardTitle>test</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="mb-3">
            <Label htmlFor="rpc-method">rpc method</Label>
            <Input
              id="rpc-method"
              placeholder="e.g. add"
              value={rpcMethod}
              onChange={(e) => setRpcMethod(e.target.value)}
            />
          </div>
          <div className="mb-3">
            <Label htmlFor="rpc-params">params (json array)</Label>
            <Textarea
              id="rpc-params"
              placeholder='[10, 20]'
              value={rpcParams}
              onChange={(e) => setRpcParams(e.target.value)}
              rows={3}
            />
          </div>
          <Button
            variant="primary"
            onClick={handleRpc}
            disabled={rpcRunning || !rpcMethod.trim()}
          >
            {rpcRunning ? "running..." : "run"}
          </Button>
          {rpcError && (
            <div className="mt-3 p-2.5 text-xs border-2 border-destructive text-destructive bg-destructive/5">
              <div className="font-medium mb-1">rpc error</div>
              <pre className="font-mono whitespace-pre-wrap">{rpcError}</pre>
            </div>
          )}
          {rpcResult && (
            <pre className="mt-3 p-3 bg-background border border-border text-xs font-mono whitespace-pre-wrap overflow-auto max-h-60">
              {rpcResult}
            </pre>
          )}
        </CardContent>
      </Card>

      {/* Logs */}
      <Card className="mb-4">
        <CardHeader>
          <CardTitle>logs</CardTitle>
        </CardHeader>
        <CardContent>
          {logs && logs.length > 0 ? (
            <pre className="p-3 bg-background border border-border text-xs font-mono whitespace-pre-wrap overflow-auto max-h-60">
              {logs.join("\n")}
            </pre>
          ) : (
            <div className="text-[13px] text-muted-foreground">
              no console output captured yet
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
