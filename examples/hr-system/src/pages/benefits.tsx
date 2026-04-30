import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Heart, Plus, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getBenefitsPlans, getBenefitsEnrollments, enrollInBenefit } from "@/server";

const MY_EMPLOYEE_ID = 1;

const TYPE_LABELS: Record<string, string> = {
  health: "Health",
  dental: "Dental",
  vision: "Vision",
  life: "Life",
  retirement: "Retirement",
};

const TYPE_COLORS: Record<string, any> = {
  health: "default",
  dental: "secondary",
  vision: "outline",
  life: "secondary",
  retirement: "default",
};

export default function BenefitsPage() {
  const [plans, setPlans] = useState<any[]>([]);
  const [enrollments, setEnrollments] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [enrolling, setEnrolling] = useState<number | null>(null);

  async function load() {
    setLoading(true);
    try {
      const [planRes, enrollRes] = await Promise.all([
        getBenefitsPlans() as any,
        getBenefitsEnrollments(MY_EMPLOYEE_ID) as any,
      ]);
      setPlans(planRes.data || []);
      setEnrollments(enrollRes.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  async function handleEnroll(planId: number) {
    setEnrolling(planId);
    try {
      const r = await enrollInBenefit(MY_EMPLOYEE_ID, planId, Date.now()) as any;
      if (r.error) { toast.error("Failed to enroll"); return; }
      toast.success("Enrolled in benefits plan");
      load();
    } finally {
      setEnrolling(null);
    }
  }

  const enrolledPlanIds = new Set(enrollments.map((e: any) => e.plan_id));

  const groupedPlans: Record<string, any[]> = {};
  for (const p of plans) {
    if (!groupedPlans[p.type]) groupedPlans[p.type] = [];
    groupedPlans[p.type].push(p);
  }

  return (
    <div>
      <PageHeader
        title="Benefits"
        description="Manage employee benefits plans and enrollments"
      />

      <Tabs defaultValue="plans">
        <TabsList className="mb-4">
          <TabsTrigger value="plans">Available Plans</TabsTrigger>
          <TabsTrigger value="my-benefits">My Benefits</TabsTrigger>
        </TabsList>

        <TabsContent value="plans">
          {loading ? (
            <TableSkeleton rows={4} cols={3} />
          ) : plans.length === 0 ? (
            <EmptyState
              icon={Heart}
              title="No benefits plans"
              description="Benefits plans will be added by your HR administrator"
            />
          ) : (
            <div className="space-y-6">
              {Object.entries(groupedPlans).map(([type, typePlans]) => (
                <div key={type}>
                  <h3 className="text-sm font-medium text-muted-foreground mb-3 flex items-center gap-2">
                    <Badge variant={TYPE_COLORS[type] || "outline"}>
                      {TYPE_LABELS[type] || type}
                    </Badge>
                  </h3>
                  <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
                    {typePlans.map((plan: any) => {
                      const enrolled = enrolledPlanIds.has(plan._id);
                      return (
                        <Card key={plan._id}>
                          <CardContent className="pt-4">
                            <div className="flex items-start justify-between mb-2">
                              <h4 className="font-semibold">{plan.name}</h4>
                              {enrolled && (
                                <Badge variant="outline" className="text-xs">Enrolled</Badge>
                              )}
                            </div>
                            {plan.provider && (
                              <p className="text-xs text-muted-foreground mb-3">
                                Provider: {plan.provider}
                              </p>
                            )}
                            <div className="space-y-1 text-sm">
                              <div className="flex justify-between">
                                <span className="text-muted-foreground">Employee cost</span>
                                <span>${plan.monthly_cost_employee}/mo</span>
                              </div>
                              <div className="flex justify-between">
                                <span className="text-muted-foreground">Employer covers</span>
                                <span>${plan.monthly_cost_employer}/mo</span>
                              </div>
                            </div>
                            <Button
                              size="sm"
                              variant={enrolled ? "outline" : "default"}
                              className="w-full mt-3"
                              disabled={enrolled || enrolling === plan._id}
                              onClick={() => handleEnroll(plan._id)}
                            >
                              {enrolling === plan._id && (
                                <Loader2 className="mr-2 h-3 w-3 animate-spin" />
                              )}
                              {enrolled ? "Enrolled" : "Enroll"}
                            </Button>
                          </CardContent>
                        </Card>
                      );
                    })}
                  </div>
                </div>
              ))}
            </div>
          )}
        </TabsContent>

        <TabsContent value="my-benefits">
          {loading ? (
            <TableSkeleton rows={3} cols={3} />
          ) : enrollments.length === 0 ? (
            <EmptyState
              icon={Heart}
              title="Not enrolled in any plans"
              description="Browse available benefits plans and enroll"
            />
          ) : (
            <div className="space-y-2">
              {enrollments.map((e: any) => {
                const plan = plans.find((p: any) => p._id === e.plan_id);
                return (
                  <div key={e._id} className="flex items-center gap-4 p-3 rounded-lg border">
                    <Heart className="h-4 w-4 text-muted-foreground flex-shrink-0" />
                    <div className="flex-1">
                      <div className="text-sm font-medium">{plan?.name || `Plan #${e.plan_id}`}</div>
                      <div className="text-xs text-muted-foreground">
                        Since {new Date(e.start_date).toLocaleDateString()}
                      </div>
                    </div>
                    {plan && (
                      <div className="text-sm text-muted-foreground">
                        ${plan.monthly_cost_employee}/mo
                      </div>
                    )}
                    <Badge variant={TYPE_COLORS[plan?.type || ""] || "outline"}>
                      {TYPE_LABELS[plan?.type || ""] || plan?.type || "—"}
                    </Badge>
                  </div>
                );
              })}
            </div>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}
