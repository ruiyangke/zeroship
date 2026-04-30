import React, { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { Briefcase, Plus, Loader2 } from "lucide-react";
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
import { Card, CardContent } from "@/components/ui/card";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import {
  getJobPostings, createJobPosting, publishJobPosting, closeJobPosting,
  getPositions,
} from "@/server";

const STATUS_COLORS: Record<string, any> = {
  draft: "secondary",
  open: "default",
  closed: "outline",
};

export default function RecruitmentPage() {
  const navigate = useNavigate();
  const [postings, setPostings] = useState<any[]>([]);
  const [positions, setPositions] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({
    position_id: "",
    title: "",
    description: "",
    requirements: "",
  });

  async function load() {
    setLoading(true);
    try {
      const r = await getJobPostings() as any;
      setPostings(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    load();
    getPositions().then((r: any) => setPositions(r.data || []));
  }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleCreate() {
    if (!form.position_id || !form.title) {
      toast.error("Position and title are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await createJobPosting(
        parseInt(form.position_id),
        form.title,
        form.description || undefined,
        form.requirements || undefined
      ) as any;
      if (r.error) { toast.error("Failed to create job posting"); return; }
      toast.success("Job posting created");
      setOpen(false);
      setForm({ position_id: "", title: "", description: "", requirements: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  async function handlePublish(id: number) {
    const r = await publishJobPosting(id) as any;
    if (r.error) { toast.error("Failed to publish"); return; }
    toast.success("Job posting published");
    load();
  }

  async function handleClose(id: number) {
    const r = await closeJobPosting(id) as any;
    if (r.error) { toast.error("Failed to close posting"); return; }
    toast.success("Job posting closed");
    load();
  }

  const columns: Column<any>[] = [
    {
      key: "title",
      header: "Job Title",
      sortable: true,
      cell: (row) => <span className="font-medium">{row.title}</span>,
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
      key: "posted_date",
      header: "Posted",
      cell: (row) =>
        row.posted_date ? new Date(row.posted_date).toLocaleDateString() : "—",
    },
    {
      key: "closing_date",
      header: "Closing",
      cell: (row) =>
        row.closing_date ? new Date(row.closing_date).toLocaleDateString() : "—",
    },
    {
      key: "actions",
      header: "",
      cell: (row) => (
        <div className="flex gap-2 justify-end" onClick={(e) => e.stopPropagation()}>
          {row.status === "draft" && (
            <Button size="sm" variant="outline" onClick={() => handlePublish(row._id)}>
              Publish
            </Button>
          )}
          {row.status === "open" && (
            <>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => navigate(`/recruitment/applicants?job=${row._id}`)}
              >
                Applicants
              </Button>
              <Button size="sm" variant="outline" onClick={() => handleClose(row._id)}>
                Close
              </Button>
            </>
          )}
        </div>
      ),
    },
  ];

  return (
    <div>
      <PageHeader
        title="Recruitment"
        description="Manage job postings and applicant pipeline"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            New Posting
          </Button>
        }
      />

      {loading ? (
        <TableSkeleton rows={5} cols={5} />
      ) : (
        <DataTable
          data={postings}
          columns={columns}
          searchable
          searchPlaceholder="Search job postings..."
          searchKeys={["title"]}
          onRowClick={(row) =>
            row.status !== "draft" &&
            navigate(`/recruitment/applicants?job=${row._id}`)
          }
          emptyState={
            <EmptyState
              icon={Briefcase}
              title="No job postings"
              description="Create your first job posting to start hiring"
              action={{ label: "Create Posting", onClick: () => setOpen(true) }}
            />
          }
        />
      )}

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>Create Job Posting</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Position *</Label>
              <Select value={form.position_id} onValueChange={(v) => set("position_id", v)}>
                <SelectTrigger>
                  <SelectValue placeholder="Select a position" />
                </SelectTrigger>
                <SelectContent>
                  {positions.map((p: any) => (
                    <SelectItem key={p._id} value={String(p._id)}>{p.title}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <Label>Job Title *</Label>
              <Input
                value={form.title}
                onChange={(e) => set("title", e.target.value)}
                placeholder="Senior Software Engineer"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Description</Label>
              <Textarea
                value={form.description}
                onChange={(e) => set("description", e.target.value)}
                placeholder="Role overview..."
                rows={3}
              />
            </div>
            <div className="space-y-1.5">
              <Label>Requirements</Label>
              <Textarea
                value={form.requirements}
                onChange={(e) => set("requirements", e.target.value)}
                placeholder="Required skills and experience..."
                rows={3}
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
    </div>
  );
}
