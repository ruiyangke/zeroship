import { useQuery } from "@tanstack/react-query";
import { getStats, getHealth, getAllUsage } from "../api";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";
import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Activity, Server, Box, Gauge, Zap } from "lucide-react";
import StatusBadge from "../components/StatusBadge";

export default function Overview() {
  const { data: stats, error: statsError } = useQuery({
    queryKey: ["stats"],
    queryFn: getStats,
    refetchInterval: 5000,
  });

  const { data: health } = useQuery({
    queryKey: ["health"],
    queryFn: getHealth,
    refetchInterval: 5000,
  });

  const { data: usage } = useQuery({
    queryKey: ["usage"],
    queryFn: getAllUsage,
    refetchInterval: 5000,
  });

  const error = statsError?.message ?? "";

  const totalRequests = (usage ?? []).reduce((sum, u) => {
    return sum + Object.values(u.counters || {}).reduce((a, b) => a + b, 0);
  }, 0);

  const utilization = stats
    ? stats.max_isolates > 0
      ? Math.round((stats.active_isolates / stats.max_isolates) * 100)
      : 0
    : 0;

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-medium tracking-[0.05em]">// overview</h1>
        <Badge variant="muted">auto-refresh: 5s</Badge>
      </div>

      {error && (
        <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3 mb-4">
          {error}
        </div>
      )}

      <div className="grid grid-cols-[repeat(auto-fit,minmax(180px,1fr))] gap-4 mb-6">
        <Card>
          <CardContent className="pt-5">
            <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.1em] text-muted-foreground mb-2">
              <Activity className="h-3 w-3" />
              health
            </div>
            <div className="text-[28px] font-bold text-primary">
              {health ? health.status : "--"}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="pt-5">
            <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.1em] text-muted-foreground mb-2">
              <Box className="h-3 w-3" />
              total apps
            </div>
            <div className="text-[28px] font-bold">
              {stats ? stats.apps.length : "--"}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="pt-5">
            <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.1em] text-muted-foreground mb-2">
              <Server className="h-3 w-3" />
              active isolates
            </div>
            <div className="text-[28px] font-bold">
              {stats ? `${stats.active_isolates} / ${stats.max_isolates}` : "--"}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="pt-5">
            <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.1em] text-muted-foreground mb-2">
              <Gauge className="h-3 w-3" />
              pool utilization
            </div>
            <div className="text-[28px] font-bold">
              {stats ? `${utilization}%` : "--"}
            </div>
          </CardContent>
        </Card>
        <Card>
          <CardContent className="pt-5">
            <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.1em] text-muted-foreground mb-2">
              <Zap className="h-3 w-3" />
              total requests
            </div>
            <div className="text-[28px] font-bold">
              {stats ? totalRequests : "--"}
            </div>
          </CardContent>
        </Card>
      </div>

      {stats && stats.apps.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>isolate pool</CardTitle>
          </CardHeader>
          <CardContent>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>app id</TableHead>
                  <TableHead>requests</TableHead>
                  <TableHead>idle</TableHead>
                  <TableHead>status</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {stats.apps.map((app) => {
                  const status: "running" | "idle" | "stopped" =
                    app.request_count > 0 && app.idle_secs < 5
                      ? "running"
                      : app.idle_secs < 60
                        ? "idle"
                        : "stopped";
                  return (
                    <TableRow key={app.app_id}>
                      <TableCell>{app.app_id}</TableCell>
                      <TableCell>{app.request_count}</TableCell>
                      <TableCell>{app.idle_secs}s</TableCell>
                      <TableCell>
                        <StatusBadge status={status} />
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
