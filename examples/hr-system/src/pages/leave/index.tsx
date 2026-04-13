import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { CalendarDays, Plus, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getLeaveRequests, requestLeave, cancelLeave, getHolidays, getEmployees } from "@/index";

const STATUS_COLORS: Record<string, any> = {
  pending: "secondary",
  approved: "default",
  denied: "destructive",
  cancelled: "outline",
};

const LEAVE_TYPES = ["vacation", "sick", "personal", "parental", "bereavement"];

// Simulated logged-in employee ID
const MY_EMPLOYEE_ID = 1;

export default function LeavePage() {
  const [requests, setRequests] = useState<any[]>([]);
  const [holidays, setHolidays] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({
    type: "",
    start_date: "",
    end_date: "",
    reason: "",
  });

  async function load() {
    setLoading(true);
    try {
      const [reqRes, holRes] = await Promise.all([
        getLeaveRequests() as any,
        getHolidays(new Date().getFullYear()) as any,
      ]);
      setRequests(reqRes.data || []);
      setHolidays(holRes.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  function calcDays(start: string, end: string) {
    if (!start || !end) return 0;
    const ms = new Date(end).getTime() - new Date(start).getTime();
    return Math.max(1, Math.ceil(ms / 86_400_000) + 1);
  }

  async function handleSubmit() {
    if (!form.type || !form.start_date || !form.end_date) {
      toast.error("Type, start date, and end date are required");
      return;
    }
    const days = calcDays(form.start_date, form.end_date);
    setSubmitting(true);
    try {
      const r = await requestLeave(
        MY_EMPLOYEE_ID,
        form.type,
        new Date(form.start_date).getTime(),
        new Date(form.end_date).getTime(),
        days,
        form.reason || undefined
      ) as any;
      if (r.error) { toast.error("Failed to submit leave request"); return; }
      toast.success("Leave request submitted");
      setOpen(false);
      setForm({ type: "", start_date: "", end_date: "", reason: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  async function handleCancel(id: number) {
    const r = await cancelLeave(id, MY_EMPLOYEE_ID) as any;
    if (r.error) { toast.error("Failed to cancel"); return; }
    toast.success("Leave cancelled");
    load();
  }

  const columns: Column<any>[] = [
    {
      key: "type",
      header: "Type",
      cell: (row) => (
        <span className="capitalize font-medium">{row.type}</span>
      ),
    },
    {
      key: "start_date",
      header: "Start Date",
      sortable: true,
      cell: (row) => new Date(row.start_date).toLocaleDateString(),
    },
    {
      key: "end_date",
      header: "End Date",
      cell: (row) => new Date(row.end_date).toLocaleDateString(),
    },
    {
      key: "days",
      header: "Days",
      cell: (row) => `${row.days} day${row.days !== 1 ? "s" : ""}`,
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
      key: "cancel",
      header: "",
      cell: (row) =>
        row.status === "pending" ? (
          <Button
            size="sm"
            variant="ghost"
            className="text-destructive hover:text-destructive"
            onClick={(e) => { e.stopPropagation(); handleCancel(row._id); }}
          >
            Cancel
          </Button>
        ) : null,
    },
  ];

  return (
    <div>
      <PageHeader
        title="Leave"
        description="Manage time-off requests and team calendar"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            Request Leave
          </Button>
        }
      />

      <Tabs defaultValue="requests">
        <TabsList className="mb-4">
          <TabsTrigger value="requests">My Requests</TabsTrigger>
          <TabsTrigger value="holidays">Holidays</TabsTrigger>
        </TabsList>

        <TabsContent value="requests">
          {loading ? (
            <TableSkeleton rows={5} cols={5} />
          ) : (
            <DataTable
              data={requests}
              columns={columns}
              searchable
              searchPlaceholder="Search leave requests..."
              searchKeys={["type", "status"]}
              emptyState={
                <EmptyState
                  icon={CalendarDays}
                  title="No leave requests"
                  description="Submit a leave request when you need time off"
                  action={{ label: "Request Leave", onClick: () => setOpen(true) }}
                />
              }
            />
          )}
        </TabsContent>

        <TabsContent value="holidays">
          {holidays.length === 0 ? (
            <EmptyState
              icon={CalendarDays}
              title="No holidays configured"
              description="Add company holidays in the Settings page"
            />
          ) : (
            <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
              {holidays.map((h: any) => (
                <div key={h._id} className="flex items-center gap-3 p-3 rounded-lg border">
                  <div className="w-10 h-10 rounded-md bg-muted flex items-center justify-center flex-shrink-0">
                    <span className="text-sm font-bold">
                      {new Date(h.date).getDate()}
                    </span>
                  </div>
                  <div>
                    <div className="text-sm font-medium">{h.name}</div>
                    <div className="text-xs text-muted-foreground">
                      {new Date(h.date).toLocaleDateString("en-US", {
                        weekday: "long", month: "long", day: "numeric",
                      })}
                    </div>
                  </div>
                  {h.is_recurring && (
                    <Badge variant="outline" className="ml-auto text-xs">Recurring</Badge>
                  )}
                </div>
              ))}
            </div>
          )}
        </TabsContent>
      </Tabs>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Request Leave</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Leave Type *</Label>
              <Select value={form.type} onValueChange={(v) => set("type", v)}>
                <SelectTrigger>
                  <SelectValue placeholder="Select type" />
                </SelectTrigger>
                <SelectContent>
                  {LEAVE_TYPES.map((t) => (
                    <SelectItem key={t} value={t} className="capitalize">{t}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid grid-cols-2 gap-4">
              <div className="space-y-1.5">
                <Label>Start Date *</Label>
                <Input
                  type="date"
                  value={form.start_date}
                  onChange={(e) => set("start_date", e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label>End Date *</Label>
                <Input
                  type="date"
                  value={form.end_date}
                  onChange={(e) => set("end_date", e.target.value)}
                />
              </div>
            </div>
            {form.start_date && form.end_date && (
              <p className="text-sm text-muted-foreground">
                Duration: {calcDays(form.start_date, form.end_date)} day(s)
              </p>
            )}
            <div className="space-y-1.5">
              <Label>Reason (optional)</Label>
              <Textarea
                value={form.reason}
                onChange={(e) => set("reason", e.target.value)}
                placeholder="Briefly explain your reason..."
                rows={2}
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={handleSubmit} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Submit
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
