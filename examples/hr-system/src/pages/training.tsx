import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { GraduationCap, Plus, Loader2, BookOpen, Clock } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getCourses, createCourse, enrollInCourse, getEnrollments } from "@/index";

const MY_EMPLOYEE_ID = 1;

const STATUS_COLORS: Record<string, any> = {
  enrolled: "secondary",
  in_progress: "default",
  completed: "default",
  dropped: "destructive",
};

export default function TrainingPage() {
  const [courses, setCourses] = useState<any[]>([]);
  const [enrollments, setEnrollments] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [enrolling, setEnrolling] = useState<number | null>(null);
  const [form, setForm] = useState({
    title: "",
    category: "",
    duration_hours: "",
    description: "",
    is_mandatory: false,
    max_participants: "",
  });

  async function load() {
    setLoading(true);
    try {
      const [courseRes, enrollRes] = await Promise.all([
        getCourses() as any,
        getEnrollments({ employee_id: MY_EMPLOYEE_ID }) as any,
      ]);
      setCourses(courseRes.data || []);
      setEnrollments(enrollRes.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string | boolean) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleCreate() {
    if (!form.title || !form.category || !form.duration_hours) {
      toast.error("Title, category, and duration are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await createCourse(
        form.title,
        form.category,
        parseFloat(form.duration_hours),
        form.description || undefined,
        form.is_mandatory,
        form.max_participants ? parseInt(form.max_participants) : undefined
      ) as any;
      if (r.error) { toast.error("Failed to create course"); return; }
      toast.success("Course created");
      setOpen(false);
      setForm({ title: "", category: "", duration_hours: "", description: "", is_mandatory: false, max_participants: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  async function handleEnroll(courseId: number) {
    setEnrolling(courseId);
    try {
      const r = await enrollInCourse(courseId, MY_EMPLOYEE_ID) as any;
      if (r.error) { toast.error("Failed to enroll"); return; }
      toast.success("Enrolled successfully");
      load();
    } finally {
      setEnrolling(null);
    }
  }

  const enrolledCourseIds = new Set(enrollments.map((e: any) => e.course_id));

  return (
    <div>
      <PageHeader
        title="Training"
        description="Course catalog and employee enrollments"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            Add Course
          </Button>
        }
      />

      <Tabs defaultValue="catalog">
        <TabsList className="mb-4">
          <TabsTrigger value="catalog">Course Catalog</TabsTrigger>
          <TabsTrigger value="my-courses">My Enrollments</TabsTrigger>
        </TabsList>

        <TabsContent value="catalog">
          {loading ? (
            <TableSkeleton rows={4} cols={2} />
          ) : courses.length === 0 ? (
            <EmptyState
              icon={GraduationCap}
              title="No courses yet"
              description="Add courses to build your training catalog"
              action={{ label: "Add Course", onClick: () => setOpen(true) }}
            />
          ) : (
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
              {courses.map((c: any) => {
                const enrolled = enrolledCourseIds.has(c._id);
                return (
                  <Card key={c._id}>
                    <CardContent className="pt-4">
                      <div className="flex items-start justify-between mb-2">
                        <div className="w-9 h-9 rounded-lg bg-primary/10 flex items-center justify-center flex-shrink-0">
                          <BookOpen className="h-4 w-4 text-primary" />
                        </div>
                        <div className="flex gap-1.5">
                          {c.is_mandatory && (
                            <Badge variant="destructive" className="text-xs">Required</Badge>
                          )}
                          <Badge variant="outline" className="text-xs">{c.category}</Badge>
                        </div>
                      </div>
                      <h3 className="font-semibold mt-2">{c.title}</h3>
                      {c.description && (
                        <p className="text-sm text-muted-foreground mt-1 line-clamp-2">
                          {c.description}
                        </p>
                      )}
                      <div className="flex items-center gap-1 mt-2 text-xs text-muted-foreground">
                        <Clock className="h-3 w-3" />
                        <span>{c.duration_hours}h</span>
                        {c.max_participants && (
                          <span className="ml-2">Max {c.max_participants} participants</span>
                        )}
                      </div>
                      <Button
                        size="sm"
                        variant={enrolled ? "outline" : "default"}
                        className="w-full mt-3"
                        disabled={enrolled || enrolling === c._id}
                        onClick={() => handleEnroll(c._id)}
                      >
                        {enrolling === c._id && <Loader2 className="mr-2 h-3 w-3 animate-spin" />}
                        {enrolled ? "Enrolled" : "Enroll"}
                      </Button>
                    </CardContent>
                  </Card>
                );
              })}
            </div>
          )}
        </TabsContent>

        <TabsContent value="my-courses">
          {loading ? (
            <TableSkeleton rows={3} cols={3} />
          ) : enrollments.length === 0 ? (
            <EmptyState
              icon={BookOpen}
              title="No enrollments"
              description="Enroll in courses from the catalog"
            />
          ) : (
            <div className="space-y-2">
              {enrollments.map((e: any) => {
                const course = courses.find((c: any) => c._id === e.course_id);
                return (
                  <div key={e._id} className="flex items-center gap-4 p-3 rounded-lg border">
                    <BookOpen className="h-4 w-4 text-muted-foreground flex-shrink-0" />
                    <div className="flex-1">
                      <div className="text-sm font-medium">
                        {course?.title || `Course #${e.course_id}`}
                      </div>
                      <div className="text-xs text-muted-foreground">
                        Enrolled {e.enrolled_at ? new Date(e.enrolled_at).toLocaleDateString() : "—"}
                      </div>
                    </div>
                    {e.score != null && (
                      <div className="text-sm font-medium">{e.score}%</div>
                    )}
                    <Badge variant={STATUS_COLORS[e.status] || "outline"}>
                      {e.status.replace("_", " ")}
                    </Badge>
                  </div>
                );
              })}
            </div>
          )}
        </TabsContent>
      </Tabs>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>Add Course</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="grid grid-cols-2 gap-4">
              <div className="col-span-2 space-y-1.5">
                <Label>Title *</Label>
                <Input
                  value={form.title}
                  onChange={(e) => set("title", e.target.value)}
                  placeholder="React Advanced Patterns"
                />
              </div>
              <div className="space-y-1.5">
                <Label>Category *</Label>
                <Input
                  value={form.category}
                  onChange={(e) => set("category", e.target.value)}
                  placeholder="Frontend"
                />
              </div>
              <div className="space-y-1.5">
                <Label>Duration (hours) *</Label>
                <Input
                  type="number"
                  value={form.duration_hours}
                  onChange={(e) => set("duration_hours", e.target.value)}
                  placeholder="8"
                />
              </div>
              <div className="space-y-1.5">
                <Label>Max Participants</Label>
                <Input
                  type="number"
                  value={form.max_participants}
                  onChange={(e) => set("max_participants", e.target.value)}
                  placeholder="20"
                />
              </div>
              <div className="flex items-center gap-2 pt-6">
                <Switch
                  checked={form.is_mandatory}
                  onCheckedChange={(v) => set("is_mandatory", v)}
                  id="mandatory"
                />
                <Label htmlFor="mandatory">Mandatory</Label>
              </div>
            </div>
            <div className="space-y-1.5">
              <Label>Description</Label>
              <Textarea
                value={form.description}
                onChange={(e) => set("description", e.target.value)}
                placeholder="Course overview..."
                rows={3}
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={handleCreate} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Add Course
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
