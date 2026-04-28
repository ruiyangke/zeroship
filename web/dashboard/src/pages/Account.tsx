// ─── Account — profile + key management ─────────────────────────
// MVP: master key visible (masked + reveal), logout button.
// Future: OAuth, billing, API keys, sessions.

import { useState } from "react";
import { TopBar } from "../workspace/components/TopBar";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Eye, EyeOff, LogOut } from "lucide-react";

interface Props {
  onLogout?: () => void;
}

export function Account({ onLogout }: Props) {
  const [reveal, setReveal] = useState(false);
  const key = (typeof localStorage !== "undefined" && localStorage.getItem("zeroship_key")) || "";

  return (
    <div className="h-screen flex flex-col">
      <TopBar onLogout={onLogout} />
      <main className="flex-1 overflow-auto" data-testid="account">
        <div className="max-w-2xl mx-auto px-6 py-12 space-y-6">
          <header>
            <h1 className="text-lg font-medium tracking-wider">// account</h1>
          </header>

          <Card>
            <CardHeader><CardTitle>master key</CardTitle></CardHeader>
            <CardContent className="space-y-3">
              <p className="text-xs text-muted-foreground">
                this key authorizes admin operations against the control plane.
                anyone with it can deploy or delete apps. don't share it.
              </p>
              <div className="grid grid-cols-[1fr_auto] gap-2 items-center">
                <code className="font-mono text-xs bg-muted p-2 break-all">
                  {key
                    ? reveal
                      ? key
                      : "*".repeat(Math.max(0, key.length - 4)) + key.slice(-4)
                    : "(not set)"}
                </code>
                <Button
                  type="button" variant="ghost" className="h-8 w-8 p-0"
                  onClick={() => setReveal(!reveal)}
                  title={reveal ? "Hide" : "Reveal"}
                  data-testid="account-toggle-reveal"
                >
                  {reveal ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
                </Button>
              </div>
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
