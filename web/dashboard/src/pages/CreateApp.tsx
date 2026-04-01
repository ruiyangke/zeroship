import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation } from "@tanstack/react-query";
import { createApp } from "../api";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";

const PLANS = ["free", "starter", "pro", "enterprise"];

export default function CreateApp() {
  const navigate = useNavigate();
  const [appId, setAppId] = useState("");
  const [planId, setPlanId] = useState("free");

  const createMutation = useMutation({
    mutationFn: () => createApp(appId.trim(), planId),
    onSuccess: (app) => navigate(`/apps/${app.id}`),
  });

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!appId.trim()) return;
    createMutation.mutate();
  }

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-medium tracking-[0.05em]">// create app</h1>
      </div>

      <Card className="max-w-[480px]">
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
    </div>
  );
}
