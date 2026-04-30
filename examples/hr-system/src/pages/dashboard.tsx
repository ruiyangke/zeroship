import React, { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  Users, Building2, CalendarDays, Briefcase, TrendingUp,
} from "lucide-react";
import {
  BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
  LineChart, Line,
} from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { StatCard } from "@/components/shared/stat-card";
import { CardSkeleton } from "@/components/shared/loading";
import { getDashboard, getHeadcountByDepartment, getDepartments } from "@/server";

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function mockTrendData() {
  const now = new Date();
  return Array.from({ length: 6 }, (_, i) => {
    const d = new Date(now.getFullYear(), now.getMonth() - 5 + i, 1);
    return {
      month: MONTHS[d.getMonth()],
      employees: 120 + Math.floor(Math.random() * 30),
    };
  });
}

export default function DashboardPage() {
  const navigate = useNavigate();
  const [stats, setStats] = useState<any>(null);
  const [headcount, setHeadcount] = useState<any[]>([]);
  const [depts, setDepts] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const trendData = React.useMemo(() => mockTrendData(), []);

  useEffect(() => {
    Promise.all([
      getDashboard().then((r: any) => setStats(r.data)),
      getHeadcountByDepartment().then((r: any) => setHeadcount(r.data || [])),
      getDepartments().then((r: any) => setDepts(r.data || [])),
    ]).finally(() => setLoading(false));
  }, []);

  const deptMap = React.useMemo(() => {
    const m: Record<number, string> = {};
    for (const d of depts) m[d._id] = d.name;
    return m;
  }, [depts]);

  const chartData = headcount
    .slice(0, 8)
    .map((row: any) => ({
      name: deptMap[row._id] || `Dept ${row._id}`,
      count: row.headcount,
    }));

  const recentActivity = [
    { text: "New employee hired: Sarah Chen", type: "hire", time: "2h ago" },
    { text: "Leave approved: John Smith (3 days)", type: "leave", time: "4h ago" },
    { text: "Payroll run completed for April 2026", type: "payroll", time: "1d ago" },
    { text: "Performance review submitted: Alice Wang", type: "review", time: "2d ago" },
    { text: "New job posting: Senior Engineer", type: "recruitment", time: "3d ago" },
  ];

  const badgeColor: Record<string, "default" | "secondary" | "outline" | "destructive"> = {
    hire: "default",
    leave: "secondary",
    payroll: "outline",
    review: "secondary",
    recruitment: "default",
  };

  return (
    <div className="space-y-6">
      {/* Stat cards */}
      <div className="grid gap-4 grid-cols-2 lg:grid-cols-4">
        {loading ? (
          Array.from({ length: 4 }).map((_, i) => <CardSkeleton key={i} />)
        ) : (
          <>
            <StatCard
              title="Total Employees"
              value={stats?.totalEmployees ?? 0}
              description="Active team members"
              icon={Users}
            />
            <StatCard
              title="Departments"
              value={stats?.totalDepartments ?? 0}
              description="Across the organization"
              icon={Building2}
            />
            <StatCard
              title="Pending Leaves"
              value={stats?.pendingLeaves ?? 0}
              description="Awaiting approval"
              icon={CalendarDays}
            />
            <StatCard
              title="Open Positions"
              value={stats?.openPositions ?? 0}
              description="Ready to hire"
              icon={Briefcase}
            />
          </>
        )}
      </div>

      {/* Charts */}
      <div className="grid gap-4 grid-cols-1 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Headcount by Department</CardTitle>
          </CardHeader>
          <CardContent>
            {loading ? (
              <div className="h-48 flex items-center justify-center text-muted-foreground text-sm">
                Loading chart...
              </div>
            ) : chartData.length === 0 ? (
              <div className="h-48 flex items-center justify-center text-muted-foreground text-sm">
                No data yet
              </div>
            ) : (
              <ResponsiveContainer width="100%" height={220}>
                <BarChart data={chartData} margin={{ top: 4, right: 4, left: -20, bottom: 0 }}>
                  <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
                  <XAxis dataKey="name" tick={{ fontSize: 11 }} className="fill-muted-foreground" />
                  <YAxis tick={{ fontSize: 11 }} className="fill-muted-foreground" />
                  <Tooltip
                    contentStyle={{
                      backgroundColor: "var(--card)",
                      border: "1px solid var(--border)",
                      borderRadius: "6px",
                      fontSize: "12px",
                    }}
                  />
                  <Bar dataKey="count" fill="var(--primary)" radius={[3, 3, 0, 0]} />
                </BarChart>
              </ResponsiveContainer>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Headcount Trend (6 months)</CardTitle>
          </CardHeader>
          <CardContent>
            <ResponsiveContainer width="100%" height={220}>
              <LineChart data={trendData} margin={{ top: 4, right: 4, left: -20, bottom: 0 }}>
                <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
                <XAxis dataKey="month" tick={{ fontSize: 11 }} className="fill-muted-foreground" />
                <YAxis tick={{ fontSize: 11 }} className="fill-muted-foreground" />
                <Tooltip
                  contentStyle={{
                    backgroundColor: "var(--card)",
                    border: "1px solid var(--border)",
                    borderRadius: "6px",
                    fontSize: "12px",
                  }}
                />
                <Line
                  type="monotone"
                  dataKey="employees"
                  stroke="var(--primary)"
                  strokeWidth={2}
                  dot={{ r: 3 }}
                />
              </LineChart>
            </ResponsiveContainer>
          </CardContent>
        </Card>
      </div>

      {/* Recent activity */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent Activity</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="space-y-3">
            {recentActivity.map((item, i) => (
              <div key={i} className="flex items-center gap-3">
                <Avatar className="h-7 w-7 flex-shrink-0">
                  <AvatarFallback className="text-xs bg-muted">
                    <TrendingUp className="h-3 w-3 text-muted-foreground" />
                  </AvatarFallback>
                </Avatar>
                <div className="flex-1 min-w-0">
                  <p className="text-sm truncate">{item.text}</p>
                </div>
                <div className="flex items-center gap-2 flex-shrink-0">
                  <Badge variant={badgeColor[item.type]} className="text-xs">
                    {item.type}
                  </Badge>
                  <span className="text-xs text-muted-foreground">{item.time}</span>
                </div>
              </div>
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
