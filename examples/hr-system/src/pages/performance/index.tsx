import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Star, Plus, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getPendingReviews, getEmployees, createReview, submitReview } from "@/index";

const STATUS_COLORS: Record<string, any> = {
  draft: "secondary",
  submitted: "default",
  acknowledged: "outline",
};

const ADMIN_ID = 1;

export default function PerformancePage() {
  const [reviews, setReviews] = useState<any[]>([]);
  const [employees, setEmployees] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [ratingOpen, setRatingOpen] = useState(false);
  const [selectedReview, setSelectedReview] = useState<any>(null);
  const [submitting, setSubmitting] = useState(false);
  const [empMap, setEmpMap] = useState<Record<number, any>>({});

  const [form, setForm] = useState({
    employee_id: "",
    period: "",
    cycle: "annual",
    strengths: "",
    improvements: "",
    goals: "",
  });
  const [rating, setRating] = useState("");

  async function load() {
    setLoading(true);
    try {
      const [revRes, empRes] = await Promise.all([
        getPendingReviews() as any,
        getEmployees() as any,
      ]);
      setReviews(revRes.data || []);
      const emps = empRes.data || [];
      setEmployees(emps);
      const m: Record<number, any> = {};
      for (const e of emps) m[e._id] = e;
      setEmpMap(m);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleCreate() {
    if (!form.employee_id || !form.period) {
      toast.error("Employee and period are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await createReview(
        parseInt(form.employee_id),
        ADMIN_ID,
        form.period,
        form.cycle,
        form.strengths || undefined,
        form.improvements || undefined,
        form.goals || undefined
      ) as any;
      if (r.error) { toast.error("Failed to create review"); return; }
      toast.success("Review created");
      setOpen(false);
      setForm({ employee_id: "", period: "", cycle: "annual", strengths: "", improvements: "", goals: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  async function handleSubmitRating() {
    if (!selectedReview || !rating) {
      toast.error("Rating is required (1–5)");
      return;
    }
    const r = parseFloat(rating);
    if (r < 1 || r > 5) { toast.error("Rating must be between 1 and 5"); return; }
    setSubmitting(true);
    try {
      const res = await submitReview(selectedReview._id, r) as any;
      if (res.error) { toast.error("Failed to submit review"); return; }
      toast.success("Review submitted");
      setRatingOpen(false);
      load();
    } finally {
      setSubmitting(false);
    }
  }

  const columns: Column<any>[] = [
    {
      key: "employee",
      header: "Employee",
      cell: (row) => {
        const emp = empMap[row.employee_id];
        return emp
          ? <span className="font-medium">{emp.first_name} {emp.last_name}</span>
          : `Employee #${row.employee_id}`;
      },
    },
    {
      key: "period",
      header: "Period",
      sortable: true,
      cell: (row) => row.period,
    },
    {
      key: "cycle",
      header: "Cycle",
      cell: (row) => <span className="capitalize">{row.cycle}</span>,
    },
    {
      key: "rating",
      header: "Rating",
      cell: (row) =>
        row.rating ? (
          <div className="flex items-center gap-1">
            <Star className="h-3.5 w-3.5 fill-yellow-400 text-yellow-400" />
            <span className="text-sm">{row.rating}/5</span>
          </div>
        ) : "—",
    },
    {
      key: "status",
      header: "Status",
      cell: (row) => (
        <Badge variant={STATUS_COLORS[row.status] || "outline"}>{row.status}</Badge>
      ),
    },
    {
      key: "actions",
      header: "",
      cell: (row) =>
        row.status === "draft" ? (
          <Button
            size="sm"
            variant="outline"
            onClick={(e) => {
              e.stopPropagation();
              setSelectedReview(row);
              setRating("");
              setRatingOpen(true);
            }}
          >
            Submit Rating
          </Button>
        ) : null,
    },
  ];

  return (
    <div>
      <PageHeader
        title="Performance Reviews"
        description="Manage employee reviews and ratings"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            New Review
          </Button>
        }
      />

      {loading ? (
        <TableSkeleton rows={5} cols={5} />
      ) : (
        <DataTable
          data={reviews}
          columns={columns}
          searchable
          searchPlaceholder="Search reviews..."
          searchKeys={["period"]}
          emptyState={
            <EmptyState
              icon={Star}
              title="No pending reviews"
              description="Create a performance review to evaluate your team"
              action={{ label: "Create Review", onClick: () => setOpen(true) }}
            />
          }
        />
      )}

      {/* Create dialog */}
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>Create Performance Review</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="grid grid-cols-2 gap-4">
              <div className="space-y-1.5">
                <Label>Employee *</Label>
                <Select value={form.employee_id} onValueChange={(v) => set("employee_id", v)}>
                  <SelectTrigger>
                    <SelectValue placeholder="Select employee" />
                  </SelectTrigger>
                  <SelectContent>
                    {employees.map((e: any) => (
                      <SelectItem key={e._id} value={String(e._id)}>
                        {e.first_name} {e.last_name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-1.5">
                <Label>Period *</Label>
                <Input
                  value={form.period}
                  onChange={(e) => set("period", e.target.value)}
                  placeholder="Q1 2026"
                />
              </div>
            </div>
            <div className="space-y-1.5">
              <Label>Cycle</Label>
              <Select value={form.cycle} onValueChange={(v) => set("cycle", v)}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="annual">Annual</SelectItem>
                  <SelectItem value="quarterly">Quarterly</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <Label>Strengths</Label>
              <Textarea
                value={form.strengths}
                onChange={(e) => set("strengths", e.target.value)}
                placeholder="Key strengths observed..."
                rows={2}
              />
            </div>
            <div className="space-y-1.5">
              <Label>Areas for Improvement</Label>
              <Textarea
                value={form.improvements}
                onChange={(e) => set("improvements", e.target.value)}
                placeholder="Areas to improve..."
                rows={2}
              />
            </div>
            <div className="space-y-1.5">
              <Label>Goals for Next Period</Label>
              <Textarea
                value={form.goals}
                onChange={(e) => set("goals", e.target.value)}
                placeholder="Goals and objectives..."
                rows={2}
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={handleCreate} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Create
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Rating dialog */}
      <Dialog open={ratingOpen} onOpenChange={setRatingOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Submit Rating</DialogTitle>
          </DialogHeader>
          <div className="py-2 space-y-4">
            <p className="text-sm text-muted-foreground">
              Submit a rating for the review period:{" "}
              <span className="font-medium text-foreground">{selectedReview?.period}</span>
            </p>
            <div className="space-y-1.5">
              <Label>Rating (1–5) *</Label>
              <Input
                type="number"
                min="1"
                max="5"
                step="0.5"
                value={rating}
                onChange={(e) => setRating(e.target.value)}
                placeholder="4.5"
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setRatingOpen(false)}>Cancel</Button>
            <Button onClick={handleSubmitRating} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Submit
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
