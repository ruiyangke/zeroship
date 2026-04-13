import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Building2, Plus, Users, Loader2, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getDepartments, createDepartment, getEmployeesByDepartment } from "@/index";

export default function DepartmentsPage() {
  const [departments, setDepartments] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({ name: "", code: "", budget: "" });

  async function load() {
    setLoading(true);
    try {
      const r = await getDepartments() as any;
      setDepartments(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleCreate() {
    if (!form.name || !form.code) {
      toast.error("Name and code are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await createDepartment(
        form.name,
        form.code,
        form.budget ? parseFloat(form.budget) : undefined
      ) as any;
      if (r.error) { toast.error("Failed to create department"); return; }
      toast.success(`${form.name} department created`);
      setOpen(false);
      setForm({ name: "", code: "", budget: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div>
      <PageHeader
        title="Departments"
        description="Manage your organization's structure"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            New Department
          </Button>
        }
      />

      {loading ? (
        <TableSkeleton rows={4} cols={3} />
      ) : departments.length === 0 ? (
        <EmptyState
          icon={Building2}
          title="No departments yet"
          description="Create departments to organize your team"
          action={{ label: "Create Department", onClick: () => setOpen(true) }}
        />
      ) : (
        <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {departments.map((dept: any) => (
            <Card key={dept._id} className="hover:bg-accent/30 transition-colors cursor-pointer">
              <CardContent className="p-5">
                <div className="flex items-start justify-between mb-3">
                  <div className="w-10 h-10 rounded-lg bg-primary/10 flex items-center justify-center">
                    <Building2 className="h-5 w-5 text-primary" />
                  </div>
                  <Badge variant="outline" className="text-xs font-mono">{dept.code}</Badge>
                </div>
                <h3 className="font-semibold">{dept.name}</h3>
                <div className="flex items-center gap-1 mt-1 text-sm text-muted-foreground">
                  <Users className="h-3.5 w-3.5" />
                  <span>{dept.headcount || 0} employees</span>
                </div>
                {dept.budget > 0 && (
                  <div className="mt-2 text-xs text-muted-foreground">
                    Budget: ${Number(dept.budget).toLocaleString()}
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
            <DialogTitle>Create Department</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Department Name *</Label>
              <Input
                value={form.name}
                onChange={(e) => set("name", e.target.value)}
                placeholder="Engineering"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Department Code *</Label>
              <Input
                value={form.code}
                onChange={(e) => set("code", e.target.value.toUpperCase())}
                placeholder="ENG"
                maxLength={10}
              />
            </div>
            <div className="space-y-1.5">
              <Label>Annual Budget (USD)</Label>
              <Input
                type="number"
                value={form.budget}
                onChange={(e) => set("budget", e.target.value)}
                placeholder="500000"
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
