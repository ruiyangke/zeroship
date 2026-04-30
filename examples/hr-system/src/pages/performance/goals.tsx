import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Target, Plus, Loader2 } from "lucide-react";
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
import { getGoals, createGoal, updateGoalProgress, getEmployees } from "@/server";

const STATUS_COLORS: Record<string, any> = {
  active: "default",
  completed: "outline",
  cancelled: "secondary",
};

const MY_EMPLOYEE_ID = 1;

export default function GoalsPage() {
  const [goals, setGoals] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({
    title: "",
    category: "performance",
    description: "",
    target_date: "",
  });

  async function load() {
    setLoading(true);
    try {
      const r = await getGoals(MY_EMPLOYEE_ID) as any;
      setGoals(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleCreate() {
    if (!form.title) { toast.error("Title is required"); return; }
    setSubmitting(true);
    try {
      const r = await createGoal(
        MY_EMPLOYEE_ID,
        form.title,
        form.category,
        form.target_date ? new Date(form.target_date).getTime() : undefined,
        form.description || undefined
      ) as any;
      if (r.error) { toast.error("Failed to create goal"); return; }
      toast.success("Goal created");
      setOpen(false);
      setForm({ title: "", category: "performance", description: "", target_date: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  async function handleProgress(id: number, progress: number) {
    const status = progress === 100 ? "completed" : "active";
    const r = await updateGoalProgress(id, progress, status) as any;
    if (r.error) { toast.error("Failed to update progress"); return; }
    setGoals((prev) =>
      prev.map((g) => g._id === id ? { ...g, progress, status } : g)
    );
  }

  return (
    <div>
      <PageHeader
        title="Goals"
        description="Track and manage performance goals"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            New Goal
          </Button>
        }
      />

      {loading ? (
        <TableSkeleton rows={4} cols={2} />
      ) : goals.length === 0 ? (
        <EmptyState
          icon={Target}
          title="No goals yet"
          description="Set goals to track your progress and development"
          action={{ label: "Create Goal", onClick: () => setOpen(true) }}
        />
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          {goals.map((g: any) => (
            <Card key={g._id}>
              <CardContent className="pt-4">
                <div className="flex items-start justify-between mb-3">
                  <div className="flex-1 min-w-0 pr-3">
                    <div className="font-medium">{g.title}</div>
                    {g.description && (
                      <div className="text-sm text-muted-foreground mt-0.5 line-clamp-2">
                        {g.description}
                      </div>
                    )}
                  </div>
                  <div className="flex gap-2 flex-shrink-0">
                    <Badge variant="outline" className="text-xs capitalize">{g.category}</Badge>
                    <Badge variant={STATUS_COLORS[g.status] || "outline"} className="text-xs">
                      {g.status}
                    </Badge>
                  </div>
                </div>

                <div className="flex items-center gap-3 mb-2">
                  <div className="flex-1 h-2 bg-muted rounded-full overflow-hidden">
                    <div
                      className="h-full bg-primary rounded-full transition-all"
                      style={{ width: `${g.progress || 0}%` }}
                    />
                  </div>
                  <span className="text-sm font-medium w-10 text-right">{g.progress || 0}%</span>
                </div>

                {g.target_date && (
                  <div className="text-xs text-muted-foreground mb-3">
                    Due: {new Date(g.target_date).toLocaleDateString()}
                  </div>
                )}

                {g.status === "active" && (
                  <div className="flex gap-2">
                    {[25, 50, 75, 100].map((p) => (
                      <Button
                        key={p}
                        size="sm"
                        variant={g.progress >= p ? "default" : "outline"}
                        className="flex-1 h-7 text-xs"
                        onClick={() => handleProgress(g._id, p)}
                      >
                        {p}%
                      </Button>
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Create Goal</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Title *</Label>
              <Input
                value={form.title}
                onChange={(e) => set("title", e.target.value)}
                placeholder="Complete TypeScript certification"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Category</Label>
              <Select value={form.category} onValueChange={(v) => set("category", v)}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="performance">Performance</SelectItem>
                  <SelectItem value="development">Development</SelectItem>
                  <SelectItem value="project">Project</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <Label>Target Date</Label>
              <Input
                type="date"
                value={form.target_date}
                onChange={(e) => set("target_date", e.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <Label>Description</Label>
              <Textarea
                value={form.description}
                onChange={(e) => set("description", e.target.value)}
                placeholder="Describe the goal and success criteria..."
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
