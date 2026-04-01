import { Link, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { listApps, getStats } from "../api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from "@/components/ui/table";
import { Plus } from "lucide-react";
import StatusBadge from "../components/StatusBadge";

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
        <Button variant="primary" asChild>
          <Link to="/apps/new">
            <Plus className="h-3 w-3 mr-1.5" />
            new app
          </Link>
        </Button>
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
                  <TableHead>version</TableHead>
                  <TableHead>requests</TableHead>
                  <TableHead>status</TableHead>
                  <TableHead>updated</TableHead>
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
                    <TableCell>v{app.version}</TableCell>
                    <TableCell>{getAppRequests(app.id)}</TableCell>
                    <TableCell>
                      <StatusBadge status={getAppStatus(app.id)} />
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {new Date(app.updated_at).toLocaleDateString()}
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
