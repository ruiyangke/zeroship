import React, { useEffect, useState, useCallback } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { UserPlus, Users } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { PageHeader } from "@/components/shared/page-header";
import { DataTable, Column } from "@/components/shared/data-table";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getEmployees, getDepartments, searchEmployees } from "@/server";

const STATUS_COLORS: Record<string, "default" | "secondary" | "destructive" | "outline"> = {
  active: "default",
  on_leave: "secondary",
  terminated: "destructive",
};

export default function EmployeesPage() {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const [employees, setEmployees] = useState<any[]>([]);
  const [departments, setDepartments] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [deptFilter, setDeptFilter] = useState("all");
  const [statusFilter, setStatusFilter] = useState("all");

  const deptMap = React.useMemo(() => {
    const m: Record<number, string> = {};
    for (const d of departments) m[d._id] = d.name;
    return m;
  }, [departments]);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const q = searchParams.get("q");
      const filters: Record<string, unknown> = {};
      if (statusFilter !== "all") filters.status = statusFilter;
      if (deptFilter !== "all") filters.department_id = parseInt(deptFilter);

      let emps: any[];
      if (q) {
        const r = await searchEmployees(q) as any;
        emps = r.data || [];
      } else {
        const r = await getEmployees(filters) as any;
        emps = r.data || [];
      }
      setEmployees(emps);
    } catch {
      toast.error("Failed to load employees");
    } finally {
      setLoading(false);
    }
  }, [deptFilter, statusFilter, searchParams]);

  useEffect(() => {
    getDepartments().then((r: any) => setDepartments(r.data || []));
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const columns: Column<any>[] = [
    {
      key: "name",
      header: "Employee",
      sortable: true,
      cell: (row) => (
        <div className="flex items-center gap-3">
          <Avatar className="h-8 w-8">
            <AvatarFallback className="text-xs bg-muted">
              {(row.first_name?.[0] ?? "")}{(row.last_name?.[0] ?? "")}
            </AvatarFallback>
          </Avatar>
          <div>
            <div className="font-medium text-sm">{row.first_name} {row.last_name}</div>
            {row.phone && <div className="text-xs text-muted-foreground">{row.phone}</div>}
          </div>
        </div>
      ),
    },
    {
      key: "email",
      header: "Email",
      sortable: true,
      cell: (row) => <span className="text-sm text-muted-foreground">{row.email}</span>,
    },
    {
      key: "department_id",
      header: "Department",
      cell: (row) => (
        <span className="text-sm">{deptMap[row.department_id] || "—"}</span>
      ),
    },
    {
      key: "status",
      header: "Status",
      cell: (row) => (
        <Badge variant={STATUS_COLORS[row.status] || "outline"}>
          {row.status?.replace("_", " ")}
        </Badge>
      ),
    },
    {
      key: "hire_date",
      header: "Hire Date",
      sortable: true,
      cell: (row) =>
        row.hire_date
          ? new Date(row.hire_date).toLocaleDateString()
          : "—",
    },
    {
      key: "salary",
      header: "Salary",
      sortable: true,
      cell: (row) =>
        row.salary ? `$${Number(row.salary).toLocaleString()}` : "—",
    },
  ];

  return (
    <div>
      <PageHeader
        title="Employees"
        description="Manage your team members and their information"
        actions={
          <Button onClick={() => navigate("/employees/new")}>
            <UserPlus className="mr-2 h-4 w-4" />
            Hire Employee
          </Button>
        }
      />

      {/* Filters */}
      <div className="flex gap-3 mb-4">
        <Select value={deptFilter} onValueChange={setDeptFilter}>
          <SelectTrigger className="w-48">
            <SelectValue placeholder="All Departments" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All Departments</SelectItem>
            {departments.map((d: any) => (
              <SelectItem key={d._id} value={String(d._id)}>{d.name}</SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Select value={statusFilter} onValueChange={setStatusFilter}>
          <SelectTrigger className="w-40">
            <SelectValue placeholder="All Status" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All Status</SelectItem>
            <SelectItem value="active">Active</SelectItem>
            <SelectItem value="on_leave">On Leave</SelectItem>
            <SelectItem value="terminated">Terminated</SelectItem>
          </SelectContent>
        </Select>
      </div>

      {loading ? (
        <TableSkeleton rows={8} cols={6} />
      ) : (
        <DataTable
          data={employees}
          columns={columns}
          searchable
          searchPlaceholder="Search by name or email..."
          searchKeys={["first_name", "last_name", "email"]}
          onRowClick={(row) => navigate(`/employees/${row._id}`)}
          emptyState={
            <EmptyState
              icon={Users}
              title="No employees found"
              description="Get started by hiring your first team member"
              action={{ label: "Hire Employee", onClick: () => navigate("/employees/new") }}
            />
          }
        />
      )}
    </div>
  );
}
