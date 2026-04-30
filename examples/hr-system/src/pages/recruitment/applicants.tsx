import React, { useEffect, useState } from "react";
import { useSearchParams, useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { Users, ArrowLeft, User } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getApplicants, getJobPosting, updateApplicantStage } from "@/server";

const STAGES = ["applied", "screening", "interview", "offer", "hired", "rejected"];

const STAGE_COLORS: Record<string, any> = {
  applied: "outline",
  screening: "secondary",
  interview: "default",
  offer: "default",
  hired: "default",
  rejected: "destructive",
};

export default function ApplicantsPage() {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const jobId = searchParams.get("job") ? parseInt(searchParams.get("job")!) : null;

  const [applicants, setApplicants] = useState<any[]>([]);
  const [jobPosting, setJobPosting] = useState<any>(null);
  const [loading, setLoading] = useState(true);
  const [stageFilter, setStageFilter] = useState("all");

  async function load() {
    if (!jobId) return;
    setLoading(true);
    try {
      const [appRes, jobRes] = await Promise.all([
        getApplicants(jobId) as any,
        getJobPosting(jobId) as any,
      ]);
      setApplicants(appRes.data || []);
      setJobPosting(jobRes.data);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, [jobId]);

  async function handleStageChange(applicantId: number, stage: string) {
    const r = await updateApplicantStage(applicantId, stage) as any;
    if (r.error) { toast.error("Failed to update stage"); return; }
    toast.success("Applicant stage updated");
    setApplicants((prev) =>
      prev.map((a) => a._id === applicantId ? { ...a, stage } : a)
    );
  }

  const filtered =
    stageFilter === "all"
      ? applicants
      : applicants.filter((a) => a.stage === stageFilter);

  // Group by stage for kanban-style columns
  const byStage = STAGES.reduce((acc, s) => {
    acc[s] = filtered.filter((a) => a.stage === s);
    return acc;
  }, {} as Record<string, any[]>);

  if (!jobId) {
    return (
      <div>
        <PageHeader title="Applicants" description="Select a job posting to view applicants" />
        <EmptyState
          icon={Users}
          title="No job selected"
          description="Go to Recruitment and select a job posting to view applicants"
          action={{ label: "View Job Postings", onClick: () => navigate("/recruitment") }}
        />
      </div>
    );
  }

  return (
    <div>
      <div className="flex items-center gap-3 mb-6">
        <Button variant="ghost" size="sm" onClick={() => navigate("/recruitment")}>
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div className="flex-1">
          <h1 className="text-2xl font-bold">
            {jobPosting ? jobPosting.title : "Applicants"}
          </h1>
          <p className="text-sm text-muted-foreground mt-0.5">
            {applicants.length} total applicants
          </p>
        </div>
        <Select value={stageFilter} onValueChange={setStageFilter}>
          <SelectTrigger className="w-40">
            <SelectValue placeholder="All Stages" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All Stages</SelectItem>
            {STAGES.map((s) => (
              <SelectItem key={s} value={s} className="capitalize">{s}</SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {loading ? (
        <TableSkeleton rows={5} cols={3} />
      ) : applicants.length === 0 ? (
        <EmptyState
          icon={Users}
          title="No applicants yet"
          description="Applicants will appear here once they apply"
        />
      ) : (
        <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-3">
          {STAGES.map((stage) => (
            <div key={stage}>
              <div className="flex items-center justify-between mb-2">
                <span className="text-xs font-medium capitalize text-muted-foreground">
                  {stage}
                </span>
                <Badge variant="outline" className="text-xs">
                  {byStage[stage].length}
                </Badge>
              </div>
              <div className="space-y-2">
                {byStage[stage].map((a: any) => (
                  <Card key={a._id} className="p-0">
                    <CardContent className="p-3">
                      <div className="flex items-center gap-2 mb-2">
                        <Avatar className="h-6 w-6">
                          <AvatarFallback className="text-xs bg-muted">
                            {a.name[0]}
                          </AvatarFallback>
                        </Avatar>
                        <div className="flex-1 min-w-0">
                          <div className="text-xs font-medium truncate">{a.name}</div>
                        </div>
                      </div>
                      <div className="text-xs text-muted-foreground truncate mb-2">{a.email}</div>
                      <Select
                        value={a.stage}
                        onValueChange={(v) => handleStageChange(a._id, v)}
                      >
                        <SelectTrigger className="h-6 text-xs">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          {STAGES.map((s) => (
                            <SelectItem key={s} value={s} className="text-xs capitalize">{s}</SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    </CardContent>
                  </Card>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
