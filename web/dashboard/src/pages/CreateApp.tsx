import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery } from "@tanstack/react-query";
import { createApp, deployApp, getTemplates } from "../api";
import type { AppTemplate } from "../api";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";

const PLANS = ["free", "pro"];

export default function CreateApp() {
  const navigate = useNavigate();
  const [appId, setAppId] = useState("");
  const [planId, setPlanId] = useState("free");
  const [selectedTemplate, setSelectedTemplate] = useState<AppTemplate | null>(null);

  const { data: templates } = useQuery({
    queryKey: ["templates"],
    queryFn: getTemplates,
  });

  const createMutation = useMutation({
    mutationFn: async () => {
      const app = await createApp(appId.trim(), planId);
      // If a template was selected, auto-deploy its code
      if (selectedTemplate) {
        await deployApp(app.id, selectedTemplate.code);
      }
      return app;
    },
    onSuccess: (app) => navigate(`/apps/${app.id}`),
  });

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!appId.trim()) return;
    createMutation.mutate();
  }

  function handleTemplateSelect(template: AppTemplate) {
    setSelectedTemplate(template);
    if (!appId.trim()) {
      setAppId(template.id);
    }
  }

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-medium tracking-[0.05em]">// create app</h1>
      </div>

      <Card className="max-w-[480px] mb-6">
        <CardContent>
          {createMutation.isError && (
            <div className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-3 mb-4">
              {createMutation.error.message}
            </div>
          )}

          <form onSubmit={handleSubmit}>
            <div className="mb-4">
              <Label htmlFor="app-id">app id</Label>
              <Input
                id="app-id"
                type="text"
                placeholder="my-app"
                value={appId}
                onChange={(e) => setAppId(e.target.value)}
                autoFocus
                pattern="[a-zA-Z0-9_-]+"
                title="alphanumeric, dashes, and underscores only"
              />
            </div>

            <div className="mb-4">
              <Label htmlFor="plan-id">plan</Label>
              <Select value={planId} onValueChange={setPlanId}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PLANS.map((p) => (
                    <SelectItem key={p} value={p}>
                      {p}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {selectedTemplate && (
              <div className="mb-4 p-3 border border-primary/30 bg-primary/5 text-xs">
                <span className="text-muted-foreground">template:</span>{" "}
                <span className="text-foreground font-medium">{selectedTemplate.name}</span>
                <button
                  type="button"
                  className="ml-2 text-muted-foreground hover:text-foreground transition-colors bg-transparent border-none cursor-pointer"
                  onClick={() => setSelectedTemplate(null)}
                >
                  clear
                </button>
              </div>
            )}

            <Button
              type="submit"
              variant="primary"
              disabled={createMutation.isPending || !appId.trim()}
            >
              {createMutation.isPending ? "creating..." : "create"}
            </Button>
          </form>
        </CardContent>
      </Card>

      {/* Templates */}
      {templates && templates.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>start from a template</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              {templates.map((t) => (
                <button
                  key={t.id}
                  type="button"
                  className={`text-left p-4 border transition-colors cursor-pointer bg-background ${
                    selectedTemplate?.id === t.id
                      ? "border-primary"
                      : "border-border hover:border-primary/50"
                  }`}
                  onClick={() => handleTemplateSelect(t)}
                >
                  <div className="text-sm font-medium mb-1">{t.name}</div>
                  <div className="text-xs text-muted-foreground">{t.description}</div>
                </button>
              ))}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
