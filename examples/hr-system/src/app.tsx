import React, { useState, useEffect } from "react";
import { createDb } from "@zeroship/db";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Button } from "@/components/ui/button";

// ---------------------------------------------------------------------------
// Server: models + data access (extracted by zeroship plugin)
// ---------------------------------------------------------------------------

const db = createDb({
  employees: {
    first_name: { type: String, required: true },
    last_name: { type: String, required: true },
    email: { type: String, required: true },
    department_id: { type: Number },
    salary: { type: Number },
    status: { type: String, default: "active" },
    skills: { type: [String] },
  },
  departments: {
    name: { type: String, required: true },
    code: { type: String, required: true },
    headcount: { type: Number, default: 0 },
  },
  leave_requests: {
    employee_id: { type: Number, required: true },
    type: { type: String, required: true },
    days: { type: Number, required: true },
    status: { type: String, default: "pending" },
  },
});

export async function getEmployees() {
  return db.employees.find({ status: "active" }).sort({ last_name: 1 });
}

export async function getDepartments() {
  return db.departments.find({}).sort({ name: 1 });
}

export async function searchEmployees(query: string) {
  return db.employees.find({ first_name: { $ilike: `%${query}%` } }).limit(20);
}

export async function getDashboardStats() {
  const { data: totalEmps } = await db.employees.count({ status: "active" });
  const { data: totalDepts } = await db.departments.count({});
  const { data: pendingLeaves } = await db.leave_requests.count({ status: "pending" });
  const { data: openPositions } = await db.employees.count({ status: "terminated" });
  return {
    data: { totalEmps, totalDepts, pendingLeaves, openPositions },
    error: null,
  };
}

export async function getHeadcountByDept() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$department_id", count: { $sum: 1 } } },
    { $sort: { count: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Client: React UI with shadcn/ui
// ---------------------------------------------------------------------------

function StatCard({ title, value, subtitle, icon }: {
  title: string; value: string | number; subtitle?: string; icon: string;
}) {
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
        <span className="text-2xl">{icon}</span>
      </CardHeader>
      <CardContent>
        <div className="text-2xl font-bold">{value}</div>
        {subtitle && <p className="text-xs text-muted-foreground">{subtitle}</p>}
      </CardContent>
    </Card>
  );
}

function EmployeeTable({ employees: emps, loading }: { employees: any[]; loading: boolean }) {
  if (loading) {
    return (
      <Card>
        <CardContent className="py-12 text-center text-muted-foreground">
          Loading employees...
        </CardContent>
      </Card>
    );
  }

  if (emps.length === 0) {
    return (
      <Card>
        <CardContent className="py-12 text-center text-muted-foreground">
          No employees found
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead className="w-[300px]">Employee</TableHead>
            <TableHead>Email</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="text-right">Skills</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {emps.map((emp: any) => (
            <TableRow key={emp._id}>
              <TableCell className="font-medium">
                <div className="flex items-center gap-3">
                  <Avatar className="h-8 w-8">
                    <AvatarFallback className="text-xs">
                      {(emp.first_name?.[0] ?? "")}{(emp.last_name?.[0] ?? "")}
                    </AvatarFallback>
                  </Avatar>
                  <div>
                    <div className="font-medium">{emp.first_name} {emp.last_name}</div>
                  </div>
                </div>
              </TableCell>
              <TableCell className="text-muted-foreground">{emp.email}</TableCell>
              <TableCell>
                <Badge variant={emp.status === "active" ? "default" : "destructive"}>
                  {emp.status}
                </Badge>
              </TableCell>
              <TableCell className="text-right">
                <div className="flex justify-end gap-1">
                  {(emp.skills || []).map((skill: string) => (
                    <Badge key={skill} variant="outline" className="text-xs">
                      {skill}
                    </Badge>
                  ))}
                </div>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Card>
  );
}

function DepartmentGrid({ departments: depts }: { departments: any[] }) {
  return (
    <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
      {depts.map((dept: any) => (
        <Card key={dept._id} className="cursor-pointer hover:bg-accent transition-colors">
          <CardContent className="p-4">
            <div className="font-semibold">{dept.name}</div>
            <div className="text-xs text-muted-foreground">{dept.code}</div>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

export default function App() {
  const [emps, setEmps] = useState<any[]>([]);
  const [depts, setDepts] = useState<any[]>([]);
  const [stats, setStats] = useState<any>(null);
  const [search, setSearch] = useState("");
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([
      getEmployees().then((r: any) => setEmps(r.data || [])),
      getDepartments().then((r: any) => setDepts(r.data || [])),
      getDashboardStats().then((r: any) => setStats(r.data)),
    ]).finally(() => setLoading(false));
  }, []);

  const handleSearch = async (q: string) => {
    setSearch(q);
    setLoading(true);
    try {
      if (q.length > 0) {
        const r = await searchEmployees(q);
        setEmps((r as any).data || []);
      } else {
        const r = await getEmployees();
        setEmps((r as any).data || []);
      }
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="min-h-screen bg-background">
      <div className="container mx-auto max-w-6xl py-8 px-4">
        {/* Header */}
        <div className="mb-8">
          <h1 className="text-3xl font-bold tracking-tight">HR Dashboard</h1>
          <p className="text-muted-foreground mt-1">
            Manage employees, departments, and leave requests
          </p>
        </div>

        {/* Stats */}
        <div className="grid gap-4 md:grid-cols-4 mb-8">
          <StatCard
            title="Total Employees"
            value={stats?.totalEmps ?? "—"}
            subtitle="Active team members"
            icon="👥"
          />
          <StatCard
            title="Departments"
            value={stats?.totalDepts ?? "—"}
            subtitle="Across the organization"
            icon="🏢"
          />
          <StatCard
            title="Pending Leaves"
            value={stats?.pendingLeaves ?? "—"}
            subtitle="Awaiting approval"
            icon="📋"
          />
          <StatCard
            title="Open Positions"
            value={stats?.openPositions ?? "—"}
            subtitle="Ready to hire"
            icon="💼"
          />
        </div>

        <Separator className="mb-8" />

        {/* Search + Employee Table */}
        <div className="mb-8">
          <div className="flex items-center justify-between mb-4">
            <h2 className="text-xl font-semibold">Employees</h2>
            <div className="w-72">
              <Input
                placeholder="Search by name..."
                value={search}
                onChange={(e) => handleSearch(e.target.value)}
              />
            </div>
          </div>
          <EmployeeTable employees={emps} loading={loading} />
        </div>

        <Separator className="mb-8" />

        {/* Departments */}
        <div>
          <h2 className="text-xl font-semibold mb-4">Departments</h2>
          <DepartmentGrid departments={depts} />
        </div>
      </div>
    </div>
  );
}
