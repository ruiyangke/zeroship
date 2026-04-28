// ─── Signup — email + password ──────────────────────────────────
//
// Mirrors Login's layout. After a successful register we don't
// auto-login (separate POST /auth/login is required) — but we do it
// on the user's behalf so they land directly in the dashboard.

import { useState, type FormEvent } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useMutation } from "@tanstack/react-query";
import {
  register as apiRegister,
  login as apiLogin,
  googleStartUrl,
} from "../api/auth";
import { useAuth } from "../auth/AuthContext";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader2 } from "lucide-react";

export default function Signup() {
  const location = useLocation();
  const navigate = useNavigate();
  const { refresh } = useAuth();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [name, setName] = useState("");

  const returnTo = sanitizeReturn(new URLSearchParams(location.search).get("return"));

  const submitMut = useMutation({
    mutationFn: async () => {
      await apiRegister({ email, password, name });
      await apiLogin({ email, password });
    },
    onSuccess: async () => {
      await refresh();
      navigate(returnTo, { replace: true });
    },
  });

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!email.trim() || password.length < 8 || !name.trim()) return;
    submitMut.mutate();
  }

  return (
    <div className="min-h-screen flex items-center justify-center p-5 bg-background">
      <div className="w-[380px] max-w-full" data-testid="signup-page">
        <h1 className="text-sm font-bold tracking-[0.18em] uppercase text-primary mb-1">
          zeroship
        </h1>
        <div className="text-xs text-muted-foreground mb-6">
          create your creator account
        </div>

        <Card>
          <CardContent>
            <form onSubmit={submit} className="space-y-3">
              <div>
                <Label htmlFor="name">name</Label>
                <Input
                  id="name" autoComplete="name" autoFocus required
                  value={name} onChange={(e) => setName(e.target.value)}
                  data-testid="signup-name"
                />
              </div>
              <div>
                <Label htmlFor="email">email</Label>
                <Input
                  id="email" type="email" autoComplete="email" required
                  value={email} onChange={(e) => setEmail(e.target.value)}
                  data-testid="signup-email"
                />
              </div>
              <div>
                <Label htmlFor="password">password</Label>
                <Input
                  id="password" type="password" autoComplete="new-password" required minLength={8}
                  value={password} onChange={(e) => setPassword(e.target.value)}
                  data-testid="signup-password"
                />
                <div className="text-[10px] text-muted-foreground mt-1">
                  at least 8 characters
                </div>
              </div>
              <Button
                type="submit" variant="primary" className="w-full"
                disabled={submitMut.isPending || !email.trim() || password.length < 8 || !name.trim()}
                data-testid="signup-submit"
              >
                {submitMut.isPending ? <Loader2 className="size-3 animate-spin mr-1" /> : null}
                {submitMut.isPending ? "creating…" : "create account"}
              </Button>
              {submitMut.isError && (
                <div className="text-xs text-destructive" data-testid="signup-error">
                  {submitMut.error.message}
                </div>
              )}
            </form>

            <div className="my-4 flex items-center gap-2 text-[10px] uppercase tracking-wider text-muted-foreground">
              <div className="flex-1 h-px bg-border" />
              or
              <div className="flex-1 h-px bg-border" />
            </div>

            <a
              href={googleStartUrl(returnTo)}
              data-testid="signup-google"
              className="flex items-center justify-center gap-2 w-full h-9 border border-border bg-card text-foreground text-xs uppercase tracking-widest hover:border-muted-foreground hover:bg-white/5 transition-colors"
            >
              continue with google
            </a>

            <div className="text-xs text-muted-foreground text-center mt-4">
              already have an account?{" "}
              <Link to="/login" className="text-primary hover:opacity-80" data-testid="signup-link-login">
                sign in
              </Link>
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

function sanitizeReturn(raw: string | null): string {
  if (!raw) return "/";
  if (raw.startsWith("//") || raw.includes("://") || !raw.startsWith("/")) return "/";
  return raw;
}
