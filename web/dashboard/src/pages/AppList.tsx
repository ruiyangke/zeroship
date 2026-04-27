import { Link, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { listApps, getStats } from "../api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from "@/components/ui/table";
import { Plus, Copy, Check, Sparkles } from "lucide-react";
import StatusBadge from "../components/StatusBadge";
import { useState } from "react";

function CopyableEndpoint({ appId }: { appId: string }) {
  const [copied, setCopied] = useState(false);
  const snippet = `curl -X POST /rpc -H 'X-App-Id: ${appId}' -d '{"jsonrpc":"2.0","method":"...","params":[],"id":1}'`;

  function handleCopy(e: React.MouseEvent) {
    e.stopPropagation();
    navigator.clipboard.writeText(snippet).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  }

  return (
    <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground font-mono">
      <code className="truncate max-w-[180px]">X-App-Id: {appId}</code>
      <button
        onClick={handleCopy}
        className="p-0.5 hover:text-primary transition-colors"
        title="Copy curl example"
      >
        {copied ? <Check className="h-3 w-3 text-green-500" /> : <Copy className="h-3 w-3" />}
      </button>
    </span>
  );
}

export default function AppList() {
  const navigate = useNavigate();

  const { data: apps, isLoading, error: appsError } = useQuery({
    queryKey: ["apps"],
    queryFn: listApps,
  });

  const { data: stats } = useQuery({
    queryKey: ["stats"],
    queryFn: getStats,
  });

  const error = appsError?.message ?? "";

  function getAppStatus(appId: string): "running" | "idle" | "stopped" {
    if (!stats) return "stopped";
    const entry = stats.apps.find((a) => a.app_id === appId);
    if (!entry) return "stopped";
    if (entry.request_count > 0 && entry.idle_secs < 5) return "running";
    if (entry.idle_secs < 60) return "idle";
    return "stopped";
  }

  function getAppRequests(appId: string): number {
    if (!stats) return 0;
    const entry = stats.apps.find((a) => a.app_id === appId);
    return entry?.request_count ?? 0;
  }

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-medium tracking-[0.05em]">// apps</h1>
        <div className="flex gap-2">
          <Button variant="primary" asChild>
            <Link to="/builder">
              <Sparkles className="h-3 w-3 mr-1.5" />
              build with ai
            </Link>
          </Button>
          <Button variant="default" asChild>
            <Link to="/apps/new">
              <Plus className="h-3 w-3 mr-1.5" />
              new app
            </Link>
          </Button>
        </div>
      </div>

      {error && (
        <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3 mb-4">
          {error}
        </div>
      )}

      {isLoading ? (
        <div className="text-[13px] text-muted-foreground py-5">loading apps...</div>
      ) : !apps || apps.length === 0 ? (
        <div className="text-[13px] text-muted-foreground py-10 text-center border border-dashed border-border">
          no apps found --{" "}
          <Link to="/apps/new" className="text-primary hover:opacity-80 transition-opacity">
            create one
          </Link>
        </div>
      ) : (
        <Card className="p-0">
          <CardContent className="p-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>id</TableHead>
                  <TableHead>plan</TableHead>
                  <TableHead>endpoint</TableHead>
                  <TableHead>requests</TableHead>
                  <TableHead>status</TableHead>
                  <TableHead>updated</TableHead>
                  <TableHead className="w-[40px]" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {apps.map((app) => (
                  <TableRow
                    key={app.id}
                    className="cursor-pointer"
                    onClick={() => navigate(`/apps/${app.id}`)}
                  >
                    <TableCell>
                      <Link
                        to={`/apps/${app.id}`}
                        onClick={(e) => e.stopPropagation()}
                        className="text-primary hover:opacity-80 transition-opacity"
                      >
                        {app.id}
                      </Link>
                    </TableCell>
                    <TableCell>
                      <Badge variant="muted">{app.plan_id}</Badge>
                    </TableCell>
                    <TableCell>
                      <CopyableEndpoint appId={app.id} />
                    </TableCell>
                    <TableCell>{getAppRequests(app.id)}</TableCell>
                    <TableCell>
                      <StatusBadge status={getAppStatus(app.id)} />
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {new Date(app.updated_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <Link
                        to={`/builder/${app.id}`}
                        onClick={(e) => e.stopPropagation()}
                        title="Open in AI builder"
                        className="inline-flex items-center justify-center h-6 w-6 hover:bg-muted text-muted-foreground hover:text-foreground transition-colors"
                      >
                        <Sparkles className="h-3 w-3" />
                      </Link>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
