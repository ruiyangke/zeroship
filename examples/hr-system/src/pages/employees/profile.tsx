import React, { useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { toast } from "sonner";
import {
  ArrowLeft, Mail, Phone, MapPin, Calendar, DollarSign,
  Building2, UserCheck, Star, FileText, AlertTriangle,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Separator } from "@/components/ui/separator";
import { LoadingSpinner } from "@/components/shared/loading";
import {
  getEmployee, getDepartment, getLeaveBalance, getLeaveRequests,
  getPayslipsByEmployee, getReviewsForEmployee, getDocuments,
  getGoals, terminateEmployee,
} from "@/index";

const STATUS_COLORS: Record<string, "default" | "secondary" | "destructive" | "outline"> = {
  active: "default",
  on_leave: "secondary",
  terminated: "destructive",
};

function InfoRow({ icon: Icon, label, value }: { icon: any; label: string; value?: string }) {
  return (
    <div className="flex items-center gap-3">
      <div className="w-8 h-8 rounded-md bg-muted flex items-center justify-center flex-shrink-0">
        <Icon className="h-4 w-4 text-muted-foreground" />
      </div>
      <div>
        <div className="text-xs text-muted-foreground">{label}</div>
        <div className="text-sm font-medium">{value || "—"}</div>
      </div>
    </div>
  );
}

export default function EmployeeProfilePage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [employee, setEmployee] = useState<any>(null);
  const [department, setDepartment] = useState<any>(null);
  const [loading, setLoading] = useState(true);
  const [leaveBalance, setLeaveBalance] = useState<any>(null);
  const [leaveHistory, setLeaveHistory] = useState<any[]>([]);
  const [payslips, setPayslips] = useState<any[]>([]);
  const [reviews, setReviews] = useState<any[]>([]);
  const [documents, setDocuments] = useState<any[]>([]);
  const [goals, setGoals] = useState<any[]>([]);
  const [terminating, setTerminating] = useState(false);

  useEffect(() => {
    if (!id) return;
    const empId = parseInt(id);

    getEmployee(empId).then((r: any) => {
      const emp = r.data;
      setEmployee(emp);
      setLoading(false);
      if (emp?.department_id) {
        getDepartment(emp.department_id).then((dr: any) => setDepartment(dr.data));
      }
    });

    Promise.all([
      getLeaveBalance(empId).then((r: any) => setLeaveBalance(r.data)),
      getLeaveRequests({ employee_id: empId }).then((r: any) => setLeaveHistory(r.data || [])),
      getPayslipsByEmployee(empId).then((r: any) => setPayslips(r.data || [])),
      getReviewsForEmployee(empId).then((r: any) => setReviews(r.data || [])),
      getDocuments(empId).then((r: any) => setDocuments(r.data || [])),
      getGoals(empId).then((r: any) => setGoals(r.data || [])),
    ]).catch(() => {});
  }, [id]);

  async function handleTerminate() {
    if (!employee) return;
    if (!confirm(`Are you sure you want to terminate ${employee.first_name} ${employee.last_name}?`)) return;
    setTerminating(true);
    try {
      const r = await terminateEmployee(parseInt(id!)) as any;
      if (r.error) { toast.error("Failed to terminate employee"); return; }
      toast.success("Employee terminated");
      setEmployee((e: any) => ({ ...e, status: "terminated" }));
    } finally {
      setTerminating(false);
    }
  }

  if (loading) return <LoadingSpinner />;
  if (!employee) return (
    <div className="text-center py-16 text-muted-foreground">Employee not found</div>
  );

  const initials = `${employee.first_name?.[0] ?? ""}${employee.last_name?.[0] ?? ""}`;

  return (
    <div>
      <div className="flex items-center gap-3 mb-6">
        <Button variant="ghost" size="sm" onClick={() => navigate("/employees")}>
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div className="flex-1" />
        {employee.status !== "terminated" && (
          <Button
            variant="outline"
            size="sm"
            className="text-destructive border-destructive hover:bg-destructive hover:text-destructive-foreground"
            onClick={handleTerminate}
            disabled={terminating}
          >
            <AlertTriangle className="mr-2 h-3 w-3" />
            Terminate
          </Button>
        )}
      </div>

      {/* Profile header */}
      <div className="flex items-start gap-6 mb-6">
        <Avatar className="h-20 w-20">
          <AvatarFallback className="text-2xl bg-primary text-primary-foreground">
            {initials}
          </AvatarFallback>
        </Avatar>
        <div className="flex-1">
          <div className="flex items-center gap-3">
            <h1 className="text-2xl font-bold">
              {employee.first_name} {employee.last_name}
            </h1>
            <Badge variant={STATUS_COLORS[employee.status] || "outline"}>
              {employee.status?.replace("_", " ")}
            </Badge>
          </div>
          <p className="text-muted-foreground mt-1">
            {department?.name || "No department"} &bull; Employee #{employee._id}
          </p>
          {(employee.skills || []).length > 0 && (
            <div className="flex gap-1.5 mt-2 flex-wrap">
              {employee.skills.map((s: string) => (
                <Badge key={s} variant="outline" className="text-xs">{s}</Badge>
              ))}
            </div>
          )}
        </div>
      </div>

      <Tabs defaultValue="overview">
        <TabsList className="mb-6">
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="performance">Performance</TabsTrigger>
          <TabsTrigger value="leave">Leave</TabsTrigger>
          <TabsTrigger value="payroll">Payroll</TabsTrigger>
          <TabsTrigger value="documents">Documents</TabsTrigger>
        </TabsList>

        {/* Overview */}
        <TabsContent value="overview">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <Card>
              <CardHeader>
                <CardTitle className="text-sm">Contact Information</CardTitle>
              </CardHeader>
              <CardContent className="space-y-3">
                <InfoRow icon={Mail} label="Email" value={employee.email} />
                <InfoRow icon={Phone} label="Phone" value={employee.phone} />
                <InfoRow icon={MapPin} label="Address" value={employee.address} />
                <InfoRow
                  icon={Calendar}
                  label="Date of Birth"
                  value={employee.date_of_birth ? new Date(employee.date_of_birth).toLocaleDateString() : undefined}
                />
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle className="text-sm">Employment Details</CardTitle>
              </CardHeader>
              <CardContent className="space-y-3">
                <InfoRow icon={Building2} label="Department" value={department?.name} />
                <InfoRow
                  icon={Calendar}
                  label="Hire Date"
                  value={employee.hire_date ? new Date(employee.hire_date).toLocaleDateString() : undefined}
                />
                <InfoRow
                  icon={DollarSign}
                  label="Salary"
                  value={employee.salary ? `$${Number(employee.salary).toLocaleString()}` : undefined}
                />
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle className="text-sm">Emergency Contact</CardTitle>
              </CardHeader>
              <CardContent className="space-y-3">
                <InfoRow icon={UserCheck} label="Name" value={employee.emergency_contact_name} />
                <InfoRow icon={Phone} label="Phone" value={employee.emergency_contact_phone} />
              </CardContent>
            </Card>

            {goals.length > 0 && (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm">Current Goals</CardTitle>
                </CardHeader>
                <CardContent className="space-y-2">
                  {goals.slice(0, 3).map((g: any) => (
                    <div key={g._id} className="flex items-center gap-3">
                      <div className="flex-1">
                        <div className="text-sm font-medium">{g.title}</div>
                        <div className="mt-1 h-1.5 bg-muted rounded-full overflow-hidden">
                          <div
                            className="h-full bg-primary rounded-full transition-all"
                            style={{ width: `${g.progress || 0}%` }}
                          />
                        </div>
                      </div>
                      <span className="text-xs text-muted-foreground">{g.progress || 0}%</span>
                    </div>
                  ))}
                </CardContent>
              </Card>
            )}
          </div>
        </TabsContent>

        {/* Performance */}
        <TabsContent value="performance">
          {reviews.length === 0 ? (
            <div className="text-center py-12 text-muted-foreground">No reviews yet</div>
          ) : (
            <div className="space-y-3">
              {reviews.map((r: any) => (
                <Card key={r._id}>
                  <CardContent className="pt-4">
                    <div className="flex items-center justify-between mb-2">
                      <div className="font-medium">{r.period}</div>
                      <div className="flex items-center gap-2">
                        {r.rating && (
                          <div className="flex items-center gap-1">
                            <Star className="h-4 w-4 fill-yellow-400 text-yellow-400" />
                            <span className="text-sm font-medium">{r.rating}/5</span>
                          </div>
                        )}
                        <Badge variant="outline">{r.status}</Badge>
                      </div>
                    </div>
                    {r.strengths && (
                      <div className="text-sm text-muted-foreground">
                        <span className="font-medium text-foreground">Strengths: </span>
                        {r.strengths}
                      </div>
                    )}
                  </CardContent>
                </Card>
              ))}
            </div>
          )}
        </TabsContent>

        {/* Leave */}
        <TabsContent value="leave">
          {leaveBalance && (
            <div className="grid grid-cols-3 gap-4 mb-6">
              {Object.entries(leaveBalance).map(([type, info]: [string, any]) => {
                if (typeof info !== "object") return null;
                const used = info.used ?? info.vacation_used ?? info.sick_used ?? info.personal_used ?? 0;
                const total = info.total ?? info.vacation_total ?? info.sick_total ?? info.personal_total ?? 20;
                return (
                  <Card key={type}>
                    <CardContent className="pt-4">
                      <div className="text-xs text-muted-foreground capitalize mb-1">{type}</div>
                      <div className="text-2xl font-bold">{total - used}</div>
                      <div className="text-xs text-muted-foreground">{used} used of {total}</div>
                      <div className="mt-2 h-1.5 bg-muted rounded-full">
                        <div
                          className="h-full bg-primary rounded-full"
                          style={{ width: `${Math.min(100, (used / total) * 100)}%` }}
                        />
                      </div>
                    </CardContent>
                  </Card>
                );
              })}
            </div>
          )}
          {leaveHistory.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground text-sm">No leave history</div>
          ) : (
            <div className="space-y-2">
              {leaveHistory.map((l: any) => (
                <div key={l._id} className="flex items-center gap-3 p-3 rounded-lg border">
                  <div className="flex-1">
                    <div className="text-sm font-medium capitalize">{l.type} Leave</div>
                    <div className="text-xs text-muted-foreground">
                      {new Date(l.start_date).toLocaleDateString()} — {new Date(l.end_date).toLocaleDateString()}
                    </div>
                  </div>
                  <div className="text-sm">{l.days} day{l.days !== 1 ? "s" : ""}</div>
                  <Badge variant={l.status === "approved" ? "default" : l.status === "denied" ? "destructive" : "secondary"}>
                    {l.status}
                  </Badge>
                </div>
              ))}
            </div>
          )}
        </TabsContent>

        {/* Payroll */}
        <TabsContent value="payroll">
          {payslips.length === 0 ? (
            <div className="text-center py-12 text-muted-foreground">No payslips yet</div>
          ) : (
            <div className="space-y-2">
              {payslips.map((p: any) => (
                <div key={p._id} className="flex items-center gap-4 p-3 rounded-lg border">
                  <DollarSign className="h-4 w-4 text-muted-foreground" />
                  <div className="flex-1">
                    <div className="text-sm font-medium">Payroll Run #{p.payroll_run_id}</div>
                    <div className="text-xs text-muted-foreground">
                      Base: ${Number(p.base_salary).toLocaleString()} &bull;
                      Tax: ${Number(p.deductions_tax).toLocaleString()}
                    </div>
                  </div>
                  <div className="font-semibold">${Number(p.net_pay).toLocaleString()}</div>
                  <Badge variant={p.status === "paid" ? "default" : "secondary"}>{p.status}</Badge>
                </div>
              ))}
            </div>
          )}
        </TabsContent>

        {/* Documents */}
        <TabsContent value="documents">
          {documents.length === 0 ? (
            <div className="text-center py-12 text-muted-foreground">No documents uploaded</div>
          ) : (
            <div className="space-y-2">
              {documents.map((d: any) => (
                <div key={d._id} className="flex items-center gap-3 p-3 rounded-lg border">
                  <FileText className="h-4 w-4 text-muted-foreground" />
                  <div className="flex-1">
                    <div className="text-sm font-medium">{d.name}</div>
                    <div className="text-xs text-muted-foreground">
                      {d.type} &bull; Uploaded {d.uploaded_at ? new Date(d.uploaded_at).toLocaleDateString() : "—"}
                    </div>
                  </div>
                  {d.expires_at && (
                    <span className="text-xs text-muted-foreground">
                      Expires {new Date(d.expires_at).toLocaleDateString()}
                    </span>
                  )}
                  <Badge variant="outline">{d.type}</Badge>
                </div>
              ))}
            </div>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}
