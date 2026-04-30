import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { CalendarDays, CheckCircle, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getLeaveRequests, approveLeave, denyLeave, getEmployees } from "@/server";

const APPROVER_ID = 1;

export default function LeaveApprovalsPage() {
  const [requests, setRequests] = useState<any[]>([]);
  const [employees, setEmployees] = useState<Record<number, any>>({});
  const [loading, setLoading] = useState(true);

  async function load() {
    setLoading(true);
    try {
      const [reqRes, empRes] = await Promise.all([
        getLeaveRequests({ status: "pending" }) as any,
        getEmployees() as any,
      ]);
      setRequests(reqRes.data || []);
      const empMap: Record<number, any> = {};
      for (const e of (empRes.data || [])) empMap[e._id] = e;
      setEmployees(empMap);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  async function handleApprove(id: number) {
    const r = await approveLeave(id, APPROVER_ID) as any;
    if (r.error) { toast.error("Failed to approve"); return; }
    toast.success("Leave approved");
    load();
  }

  async function handleDeny(id: number) {
    const r = await denyLeave(id, APPROVER_ID) as any;
    if (r.error) { toast.error("Failed to deny"); return; }
    toast.success("Leave denied");
    load();
  }

  const columns: Column<any>[] = [
    {
      key: "employee",
      header: "Employee",
      cell: (row) => {
        const emp = employees[row.employee_id];
        return emp
          ? <span className="font-medium">{emp.first_name} {emp.last_name}</span>
          : <span className="text-muted-foreground">Employee #{row.employee_id}</span>;
      },
    },
    {
      key: "type",
      header: "Type",
      cell: (row) => <span className="capitalize">{row.type}</span>,
    },
    {
      key: "start_date",
      header: "Start",
      cell: (row) => new Date(row.start_date).toLocaleDateString(),
    },
    {
      key: "end_date",
      header: "End",
      cell: (row) => new Date(row.end_date).toLocaleDateString(),
    },
    {
      key: "days",
      header: "Days",
      cell: (row) => `${row.days}`,
    },
    {
      key: "reason",
      header: "Reason",
      cell: (row) => (
        <span className="text-sm text-muted-foreground truncate max-w-xs block">
          {row.reason || "—"}
        </span>
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
            className="text-green-600 border-green-200 hover:bg-green-50 hover:text-green-700"
            onClick={() => handleApprove(row._id)}
          >
            <CheckCircle className="mr-1.5 h-3 w-3" />
            Approve
          </Button>
          <Button
            size="sm"
            variant="outline"
            className="text-destructive border-destructive/30 hover:bg-destructive hover:text-destructive-foreground"
            onClick={() => handleDeny(row._id)}
          >
            <XCircle className="mr-1.5 h-3 w-3" />
            Deny
          </Button>
        </div>
      ),
    },
  ];

  return (
    <div>
      <PageHeader
        title="Leave Approvals"
        description="Review and action pending leave requests"
      />

      {loading ? (
        <TableSkeleton rows={5} cols={6} />
      ) : (
        <DataTable
          data={requests}
          columns={columns}
          emptyState={
            <EmptyState
              icon={CalendarDays}
              title="No pending approvals"
              description="All leave requests have been reviewed"
            />
          }
        />
      )}
    </div>
  );
}
