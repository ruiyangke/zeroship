import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Settings, Plus, Loader2, Building2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Separator } from "@/components/ui/separator";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getHolidays, createHoliday } from "@/index";

export default function SettingsPage() {
  const [holidays, setHolidays] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({ name: "", date: "", is_recurring: false });

  // Company settings (mock, no backend for these yet)
  const [companyName, setCompanyName] = useState("Acme Corp");
  const [timezone, setTimezone] = useState("America/New_York");
  const [fiscalYear, setFiscalYear] = useState("January");
  const [workHours, setWorkHours] = useState("8");

  async function loadHolidays() {
    setLoading(true);
    try {
      const r = await getHolidays(new Date().getFullYear()) as any;
      setHolidays(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { loadHolidays(); }, []);

  async function handleAddHoliday() {
    if (!form.name || !form.date) {
      toast.error("Name and date are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await createHoliday(
        form.name,
        new Date(form.date).getTime(),
        form.is_recurring
      ) as any;
      if (r.error) { toast.error("Failed to add holiday"); return; }
      toast.success("Holiday added");
      setOpen(false);
      setForm({ name: "", date: "", is_recurring: false });
      loadHolidays();
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div>
      <PageHeader title="Settings" description="Configure company settings and preferences" />

      <Tabs defaultValue="company">
        <TabsList className="mb-6">
          <TabsTrigger value="company">Company</TabsTrigger>
          <TabsTrigger value="holidays">Holidays</TabsTrigger>
          <TabsTrigger value="policies">Leave Policies</TabsTrigger>
        </TabsList>

        {/* Company Settings */}
        <TabsContent value="company">
          <div className="space-y-4 max-w-xl">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Company Information</CardTitle>
                <CardDescription>Basic information about your organization</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4">
                <div className="space-y-1.5">
                  <Label>Company Name</Label>
                  <Input
                    value={companyName}
                    onChange={(e) => setCompanyName(e.target.value)}
                  />
                </div>
                <div className="space-y-1.5">
                  <Label>Timezone</Label>
                  <Input
                    value={timezone}
                    onChange={(e) => setTimezone(e.target.value)}
                  />
                </div>
                <div className="grid grid-cols-2 gap-4">
                  <div className="space-y-1.5">
                    <Label>Fiscal Year Start</Label>
                    <Input
                      value={fiscalYear}
                      onChange={(e) => setFiscalYear(e.target.value)}
                    />
                  </div>
                  <div className="space-y-1.5">
                    <Label>Standard Work Hours/Day</Label>
                    <Input
                      type="number"
                      value={workHours}
                      onChange={(e) => setWorkHours(e.target.value)}
                    />
                  </div>
                </div>
                <Button onClick={() => toast.success("Settings saved")}>
                  Save Changes
                </Button>
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle className="text-base">Notifications</CardTitle>
                <CardDescription>Configure notification preferences</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4">
                {[
                  { label: "Email notifications for leave approvals", key: "leave" },
                  { label: "Payroll completion alerts", key: "payroll" },
                  { label: "Review deadline reminders", key: "reviews" },
                  { label: "Document expiry warnings", key: "docs" },
                ].map((item) => (
                  <div key={item.key} className="flex items-center justify-between">
                    <Label className="font-normal">{item.label}</Label>
                    <Switch defaultChecked />
                  </div>
                ))}
              </CardContent>
            </Card>
          </div>
        </TabsContent>

        {/* Holidays */}
        <TabsContent value="holidays">
          <div className="flex items-center justify-between mb-4">
            <p className="text-sm text-muted-foreground">
              {new Date().getFullYear()} company holidays
            </p>
            <Button size="sm" onClick={() => setOpen(true)}>
              <Plus className="mr-2 h-3.5 w-3.5" />
              Add Holiday
            </Button>
          </div>

          {loading ? (
            <TableSkeleton rows={4} cols={2} />
          ) : holidays.length === 0 ? (
            <EmptyState
              icon={Settings}
              title="No holidays configured"
              description="Add company holidays to your calendar"
              action={{ label: "Add Holiday", onClick: () => setOpen(true) }}
            />
          ) : (
            <div className="space-y-2 max-w-xl">
              {holidays.map((h: any) => (
                <div key={h._id} className="flex items-center gap-3 p-3 rounded-lg border">
                  <div className="w-10 h-10 rounded-md bg-muted flex items-center justify-center text-sm font-bold flex-shrink-0">
                    {new Date(h.date).getDate()}
                  </div>
                  <div className="flex-1">
                    <div className="text-sm font-medium">{h.name}</div>
                    <div className="text-xs text-muted-foreground">
                      {new Date(h.date).toLocaleDateString("en-US", {
                        weekday: "long", month: "long", day: "numeric",
                      })}
                    </div>
                  </div>
                  {h.is_recurring && (
                    <Badge variant="outline" className="text-xs">Recurring</Badge>
                  )}
                </div>
              ))}
            </div>
          )}
        </TabsContent>

        {/* Leave Policies */}
        <TabsContent value="policies">
          <div className="space-y-4 max-w-xl">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Default Leave Allocations</CardTitle>
                <CardDescription>Annual leave days per employee</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4">
                {[
                  { label: "Vacation", value: "20 days" },
                  { label: "Sick Leave", value: "10 days" },
                  { label: "Personal Leave", value: "5 days" },
                  { label: "Parental Leave", value: "90 days" },
                  { label: "Bereavement", value: "5 days" },
                ].map((item) => (
                  <div key={item.label} className="flex items-center justify-between">
                    <Label className="font-normal">{item.label}</Label>
                    <span className="text-sm font-medium">{item.value}</span>
                  </div>
                ))}
                <Separator />
                <Button variant="outline" onClick={() => toast.info("Policy configuration coming soon")}>
                  Edit Policies
                </Button>
              </CardContent>
            </Card>
          </div>
        </TabsContent>
      </Tabs>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Add Holiday</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Holiday Name *</Label>
              <Input
                value={form.name}
                onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
                placeholder="Independence Day"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Date *</Label>
              <Input
                type="date"
                value={form.date}
                onChange={(e) => setForm((f) => ({ ...f, date: e.target.value }))}
              />
            </div>
            <div className="flex items-center gap-2">
              <Switch
                checked={form.is_recurring}
                onCheckedChange={(v) => setForm((f) => ({ ...f, is_recurring: v }))}
                id="recurring"
              />
              <Label htmlFor="recurring">Recurring yearly</Label>
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={handleAddHoliday} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Add
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
