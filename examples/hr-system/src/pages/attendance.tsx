import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Clock, CheckCircle, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getEmployees, getTimesheets, clockIn, clockOut, approveTimesheet, submitTimesheet } from "@/index";

const STATUS_COLORS: Record<string, any> = {
  draft: "secondary",
  submitted: "default",
  approved: "default",
};

export default function AttendancePage() {
  const [employees, setEmployees] = useState<any[]>([]);
  const [timesheets, setTimesheets] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [selectedEmp, setSelectedEmp] = useState<number | null>(null);
  const [empTimesheets, setEmpTimesheets] = useState<any[]>([]);
  const [loadingTs, setLoadingTs] = useState(false);

  useEffect(() => {
    getEmployees({ status: "active" }).then((r: any) => {
      setEmployees(r.data || []);
      setLoading(false);
    });
  }, []);

  async function loadTimesheets(empId: number) {
    setLoadingTs(true);
    setSelectedEmp(empId);
    try {
      const r = await getTimesheets(empId) as any;
      setEmpTimesheets(r.data || []);
    } finally {
      setLoadingTs(false);
    }
  }

  async function handleClockIn(empId: number) {
    const today = new Date();
    today.setHours(0, 0, 0, 0);
    const r = await clockIn(empId, today.getTime()) as any;
    if (r.error) { toast.error(r.error.message || "Failed to clock in"); return; }
    toast.success("Clocked in successfully");
    if (selectedEmp === empId) loadTimesheets(empId);
  }

  async function handleApprove(id: number) {
    const r = await approveTimesheet(id) as any;
    if (r.error) { toast.error("Failed to approve"); return; }
    toast.success("Timesheet approved");
    if (selectedEmp) loadTimesheets(selectedEmp);
  }

  const empColumns: Column<any>[] = [
    {
      key: "name",
      header: "Employee",
      cell: (row) => (
        <div className="flex items-center gap-3">
          <Avatar className="h-8 w-8">
            <AvatarFallback className="text-xs bg-muted">
              {(row.first_name?.[0] ?? "")}{(row.last_name?.[0] ?? "")}
            </AvatarFallback>
          </Avatar>
          <span className="font-medium text-sm">{row.first_name} {row.last_name}</span>
        </div>
      ),
    },
    {
      key: "status",
      header: "Status",
      cell: (row) => (
        <Badge variant={row.status === "active" ? "default" : "secondary"}>
          {row.status}
        </Badge>
      ),
    },
    {
      key: "actions",
      header: "",
      cell: (row) => (
        <div className="flex gap-2 justify-end" onClick={(e) => e.stopPropagation()}>
          <Button
            size="sm"
            variant="outline"
            onClick={() => handleClockIn(row._id)}
          >
            <Clock className="mr-1.5 h-3 w-3" />
            Clock In
          </Button>
        </div>
      ),
    },
  ];

  const tsColumns: Column<any>[] = [
    {
      key: "date",
      header: "Date",
      sortable: true,
      cell: (row) => new Date(row.date).toLocaleDateString(),
    },
    {
      key: "clock_in",
      header: "Clock In",
      cell: (row) => row.clock_in ? new Date(row.clock_in).toLocaleTimeString() : "—",
    },
    {
      key: "clock_out",
      header: "Clock Out",
      cell: (row) => row.clock_out ? new Date(row.clock_out).toLocaleTimeString() : "—",
    },
    {
      key: "hours_worked",
      header: "Hours",
      cell: (row) => row.hours_worked ? `${row.hours_worked}h` : "—",
    },
    {
      key: "overtime_hours",
      header: "Overtime",
      cell: (row) =>
        row.overtime_hours > 0 ? (
          <span className="text-amber-500">{row.overtime_hours}h</span>
        ) : "—",
    },
    {
      key: "status",
      header: "Status",
      cell: (row) => (
        <Badge variant={STATUS_COLORS[row.status] || "outline"}>
          {row.status}
        </Badge>
      ),
    },
    {
      key: "approve",
      header: "",
      cell: (row) => (
        row.status === "submitted" ? (
          <Button
            size="sm"
            variant="outline"
            onClick={(e) => { e.stopPropagation(); handleApprove(row._id); }}
          >
            <CheckCircle className="mr-1.5 h-3 w-3" />
            Approve
          </Button>
        ) : null
      ),
    },
  ];

  return (
    <div className="space-y-6">
      <PageHeader
        title="Attendance"
        description="Track employee time and manage timesheets"
      />

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <div>
          <h2 className="text-sm font-medium text-muted-foreground mb-3">
            Employees — click to view timesheets
          </h2>
          {loading ? (
            <TableSkeleton rows={5} cols={3} />
          ) : (
            <DataTable
              data={employees}
              columns={empColumns}
              searchable
              searchPlaceholder="Search employees..."
              searchKeys={["first_name", "last_name"]}
              onRowClick={(row) => loadTimesheets(row._id)}
              emptyState={
                <EmptyState
                  icon={Clock}
                  title="No employees"
                  description="Add employees to track attendance"
                />
              }
            />
          )}
        </div>

        <div>
          <h2 className="text-sm font-medium text-muted-foreground mb-3">
            {selectedEmp ? "Timesheets" : "Select an employee to view timesheets"}
          </h2>
          {loadingTs ? (
            <TableSkeleton rows={5} cols={4} />
          ) : selectedEmp ? (
            <DataTable
              data={empTimesheets}
              columns={tsColumns}
              emptyState={
                <EmptyState
                  icon={Clock}
                  title="No timesheets"
                  description="No timesheet entries for this employee yet"
                />
              }
            />
          ) : (
            <Card>
              <CardContent className="py-16 text-center text-sm text-muted-foreground">
                Select an employee from the left to view their timesheets
              </CardContent>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}
