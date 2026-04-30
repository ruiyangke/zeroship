import React, { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { DollarSign, Plus, Loader2, Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getPayrollRuns, exportPayroll } from "@/server";

const STATUS_COLORS: Record<string, any> = {
  draft: "secondary",
  processing: "default",
  completed: "default",
};

export default function PayrollPage() {
  const navigate = useNavigate();
  const [runs, setRuns] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);

  async function load() {
    setLoading(true);
    try {
      const r = await getPayrollRuns() as any;
      setRuns(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  async function handleExport(id: number, period: string) {
    const r = await exportPayroll(id) as any;
    if (r.error || !r.data) { toast.error("Failed to export"); return; }
    const blob = new Blob([r.data], { type: "text/csv" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `payroll-${period}.csv`;
    a.click();
    URL.revokeObjectURL(url);
    toast.success("Exported successfully");
  }

  const columns: Column<any>[] = [
    {
      key: "period",
      header: "Period",
      sortable: true,
      cell: (row) => <span className="font-medium">{row.period}</span>,
    },
    {
      key: "run_date",
      header: "Run Date",
      sortable: true,
      cell: (row) => row.run_date ? new Date(row.run_date).toLocaleDateString() : "—",
    },
    {
      key: "status",
      header: "Status",
      cell: (row) => (
        <Badge variant={STATUS_COLORS[row.status] || "outline"}>{row.status}</Badge>
      ),
    },
    {
      key: "total_gross",
      header: "Gross",
      cell: (row) =>
        row.total_gross ? `$${Number(row.total_gross).toLocaleString()}` : "—",
    },
    {
      key: "total_deductions",
      header: "Deductions",
      cell: (row) =>
        row.total_deductions ? `$${Number(row.total_deductions).toLocaleString()}` : "—",
    },
    {
      key: "total_net",
      header: "Net Pay",
      cell: (row) => (
        <span className="font-semibold">
          {row.total_net ? `$${Number(row.total_net).toLocaleString()}` : "—"}
        </span>
      ),
    },
    {
      key: "actions",
      header: "",
      cell: (row) => (
        <div className="flex gap-2 justify-end" onClick={(e) => e.stopPropagation()}>
          {row.status === "completed" && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => handleExport(row._id, row.period)}
            >
              <Download className="h-3.5 w-3.5" />
            </Button>
          )}
        </div>
      ),
    },
  ];

  return (
    <div>
      <PageHeader
        title="Payroll"
        description="Manage payroll runs and payslips"
        actions={
          <Button onClick={() => navigate("/payroll/run")}>
            <Plus className="mr-2 h-4 w-4" />
            Run Payroll
          </Button>
        }
      />

      {loading ? (
        <TableSkeleton rows={5} cols={6} />
      ) : (
        <DataTable
          data={runs}
          columns={columns}
          searchable
          searchPlaceholder="Search by period..."
          searchKeys={["period"]}
          emptyState={
            <EmptyState
              icon={DollarSign}
              title="No payroll runs"
              description="Run payroll to generate payslips for your team"
              action={{ label: "Run Payroll", onClick: () => navigate("/payroll/run") }}
            />
          }
        />
      )}
    </div>
  );
}
