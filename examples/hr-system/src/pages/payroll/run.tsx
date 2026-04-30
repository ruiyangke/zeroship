import React, { useState } from "react";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { ArrowLeft, DollarSign, Loader2, CheckCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { createPayrollRun, processPayroll, finalizePayroll } from "@/server";

const STEPS = ["Configure", "Process", "Review", "Finalize"] as const;

const ADMIN_ID = 1;

export default function RunPayrollPage() {
  const navigate = useNavigate();
  const [step, setStep] = useState(0);
  const [period, setPeriod] = useState(() => {
    const now = new Date();
    return `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}`;
  });
  const [runId, setRunId] = useState<number | null>(null);
  const [result, setResult] = useState<any>(null);
  const [loading, setLoading] = useState(false);

  async function handleCreate() {
    if (!period) { toast.error("Period is required"); return; }
    setLoading(true);
    try {
      const r = await createPayrollRun(period, ADMIN_ID) as any;
      if (r.error) { toast.error(r.error.message || "Failed to create run"); return; }
      setRunId(r.data._id);
      setStep(1);
    } finally {
      setLoading(false);
    }
  }

  async function handleProcess() {
    if (!runId) return;
    setLoading(true);
    try {
      const r = await processPayroll(runId) as any;
      if (r.error) { toast.error("Failed to process payroll"); return; }
      setResult(r.data);
      setStep(2);
    } finally {
      setLoading(false);
    }
  }

  async function handleFinalize() {
    if (!runId) return;
    setLoading(true);
    try {
      const r = await finalizePayroll(runId) as any;
      if (r.error) { toast.error("Failed to finalize"); return; }
      setStep(3);
      toast.success("Payroll finalized and payslips marked as paid");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="max-w-xl">
      <div className="flex items-center gap-3 mb-6">
        <Button variant="ghost" size="sm" onClick={() => navigate("/payroll")}>
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div>
          <h1 className="text-2xl font-bold">Run Payroll</h1>
          <p className="text-sm text-muted-foreground mt-0.5">
            Process payroll for your team
          </p>
        </div>
      </div>

      {/* Stepper */}
      <div className="flex items-center mb-8">
        {STEPS.map((s, i) => (
          <React.Fragment key={s}>
            <div className="flex items-center gap-2">
              <div className={`w-7 h-7 rounded-full flex items-center justify-center text-xs font-semibold
                ${i < step ? "bg-primary text-primary-foreground" :
                  i === step ? "border-2 border-primary text-primary" :
                  "bg-muted text-muted-foreground"}`}>
                {i < step ? <CheckCircle className="h-4 w-4" /> : i + 1}
              </div>
              <span className={`text-sm ${i === step ? "font-medium" : "text-muted-foreground"}`}>
                {s}
              </span>
            </div>
            {i < STEPS.length - 1 && (
              <div className={`flex-1 h-px mx-3 ${i < step ? "bg-primary" : "bg-border"}`} />
            )}
          </React.Fragment>
        ))}
      </div>

      {/* Step 0: Configure */}
      {step === 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Configure Payroll Run</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="space-y-1.5">
              <Label>Period *</Label>
              <Input
                type="month"
                value={period}
                onChange={(e) => setPeriod(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                e.g. 2026-04 for April 2026
              </p>
            </div>
            <Button onClick={handleCreate} disabled={loading} className="w-full">
              {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Create Payroll Run
            </Button>
          </CardContent>
        </Card>
      )}

      {/* Step 1: Process */}
      {step === 1 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Process Payroll</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="rounded-lg bg-muted p-4 space-y-2 text-sm">
              <div className="flex justify-between">
                <span className="text-muted-foreground">Period</span>
                <span className="font-medium">{period}</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">Run ID</span>
                <span className="font-mono text-xs">#{runId}</span>
              </div>
            </div>
            <p className="text-sm text-muted-foreground">
              This will calculate salaries, deductions, and net pay for all active employees.
            </p>
            <Button onClick={handleProcess} disabled={loading} className="w-full">
              {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Process Payroll
            </Button>
          </CardContent>
        </Card>
      )}

      {/* Step 2: Review */}
      {step === 2 && result && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Review Results</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="grid grid-cols-3 gap-3">
              <div className="rounded-lg bg-muted p-3 text-center">
                <div className="text-2xl font-bold">{result.processed}</div>
                <div className="text-xs text-muted-foreground">Employees</div>
              </div>
              <div className="rounded-lg bg-muted p-3 text-center">
                <div className="text-lg font-bold">${Number(result.total_gross).toLocaleString()}</div>
                <div className="text-xs text-muted-foreground">Gross</div>
              </div>
              <div className="rounded-lg bg-muted p-3 text-center">
                <div className="text-lg font-bold">${Number(result.total_net).toLocaleString()}</div>
                <div className="text-xs text-muted-foreground">Net Pay</div>
              </div>
            </div>
            <Separator />
            <p className="text-sm text-muted-foreground">
              Review the numbers above. Click Finalize to mark all payslips as paid.
            </p>
            <Button onClick={handleFinalize} disabled={loading} className="w-full">
              {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Finalize &amp; Pay
            </Button>
          </CardContent>
        </Card>
      )}

      {/* Step 3: Done */}
      {step === 3 && (
        <Card>
          <CardContent className="py-12 text-center">
            <CheckCircle className="h-12 w-12 text-green-500 mx-auto mb-4" />
            <h2 className="text-xl font-semibold mb-2">Payroll Complete</h2>
            <p className="text-muted-foreground text-sm mb-6">
              Payroll for {period} has been processed and all payslips are marked as paid.
            </p>
            <Button onClick={() => navigate("/payroll")}>
              Back to Payroll
            </Button>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
