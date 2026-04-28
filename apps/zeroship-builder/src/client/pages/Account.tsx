// ─── Account — profile + key management ─────────────────────────
// MVP: master key visible (masked + reveal), logout button.
// Future: OAuth, billing, API keys, sessions.

import { TopBar } from "../workspace/components/TopBar";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { LogOut } from "lucide-react";
import { useAuth } from "../auth/AuthContext";

interface Props {
  onLogout?: () => void;
}

export function Account({ onLogout }: Props) {
  const { user } = useAuth();

  return (
    <div className="h-screen flex flex-col">
      <TopBar onLogout={onLogout} />
      <main className="flex-1 overflow-auto" data-testid="account">
        <div className="max-w-2xl mx-auto px-6 py-12 space-y-6">
          <header>
            <h1 className="text-lg font-medium tracking-wider">// account</h1>
          </header>

          <Card>
            <CardHeader><CardTitle>profile</CardTitle></CardHeader>
            <CardContent className="space-y-3">
              <Field label="name"  value={user?.name  ?? "—"} testid="account-name" />
              <Field label="email" value={user?.email ?? "—"} testid="account-email" />
              <Field label="id"    value={user?.id    ?? "—"} testid="account-id" mono />
            </CardContent>
          </Card>

          <Card>
            <CardHeader><CardTitle>session</CardTitle></CardHeader>
            <CardContent>
              <Button
                type="button" variant="destructive"
                onClick={onLogout}
                data-testid="account-logout"
              >
                <LogOut className="size-3 mr-1.5" /> logout
              </Button>
            </CardContent>
          </Card>
        </div>
      </main>
    </div>
  );
}

function Field({
  label, value, testid, mono = false,
}: { label: string; value: string; testid: string; mono?: boolean }) {
  return (
    <div className="grid grid-cols-[80px_1fr] gap-3 items-center">
      <span className="text-[10px] uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      <span
        data-testid={testid}
        className={`text-sm ${mono ? "font-mono text-xs" : ""}`}
      >
        {value}
      </span>
    </div>
  );
}
