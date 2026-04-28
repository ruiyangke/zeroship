// ─── Login — email+password + Google ────────────────────────────
//
// Single-column auth page. Email + password form on top, divider,
// "Continue with Google" button below. Errors from /auth/login
// surface inline. After success, refreshes the auth context and
// bounces to "/" (or whatever ?return param the URL has).

import { useState, type FormEvent } from "react";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { useMutation } from "@tanstack/react-query";
import { login as apiLogin, googleStartUrl } from "../api/auth";
import { useAuth } from "../auth/AuthContext";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader2 } from "lucide-react";

interface Props { onLogin?: () => void }

export default function Login({ onLogin }: Props) {
  const location = useLocation();
  const navigate = useNavigate();
  const { refresh } = useAuth();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");

  const params = new URLSearchParams(location.search);
  const returnTo = sanitizeReturn(params.get("return"));
  const oauthError = params.get("error");

  const loginMut = useMutation({
    mutationFn: () => apiLogin({ email, password }),
    onSuccess: async () => {
      await refresh();
      onLogin?.();
      navigate(returnTo, { replace: true });
    },
  });

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!email.trim() || !password) return;
    loginMut.mutate();
  }

  return (
    <div className="min-h-screen flex items-center justify-center p-5 bg-background">
      <div className="w-[380px] max-w-full" data-testid="login-page">
        <h1 className="text-sm font-bold tracking-[0.18em] uppercase text-primary mb-1">
          zeroship
        </h1>
        <div className="text-xs text-muted-foreground mb-6">
          sign in to your creator account
        </div>

        <Card>
          <CardContent>
            {oauthError && (
              <div
                className="text-xs text-destructive border border-destructive/30 bg-destructive/5 p-2 mb-4"
                data-testid="login-oauth-error"
              >
                google sign-in: {oauthError}
              </div>
            )}

            <form onSubmit={submit} className="space-y-3">
              <div>
                <Label htmlFor="email">email</Label>
                <Input
                  id="email" type="email" autoComplete="email" autoFocus required
                  value={email} onChange={(e) => setEmail(e.target.value)}
                  data-testid="login-email"
                />
              </div>
              <div>
                <Label htmlFor="password">password</Label>
                <Input
                  id="password" type="password" autoComplete="current-password" required
                  value={password} onChange={(e) => setPassword(e.target.value)}
                  data-testid="login-password"
                />
              </div>
              <Button
                type="submit" variant="primary" className="w-full"
                disabled={loginMut.isPending || !email.trim() || !password}
                data-testid="login-submit"
              >
                {loginMut.isPending ? <Loader2 className="size-3 animate-spin mr-1" /> : null}
                {loginMut.isPending ? "signing in…" : "sign in"}
              </Button>
              {loginMut.isError && (
                <div className="text-xs text-destructive" data-testid="login-error">
                  {loginMut.error.message}
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
              data-testid="login-google"
              className="flex items-center justify-center gap-2 w-full h-9 border border-border bg-card text-foreground text-xs uppercase tracking-widest hover:border-muted-foreground hover:bg-white/5 transition-colors"
            >
              <GoogleG />
              continue with google
            </a>

            <div className="text-xs text-muted-foreground text-center mt-4">
              no account?{" "}
              <Link to="/signup" className="text-primary hover:opacity-80" data-testid="login-link-signup">
                create one
              </Link>
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

/** Reject anything that isn't a same-origin path. */
function sanitizeReturn(raw: string | null): string {
  if (!raw) return "/";
  if (raw.startsWith("//") || raw.includes("://") || !raw.startsWith("/")) return "/";
  return raw;
}

function GoogleG() {
  // Inlined SVG so we don't pull a CDN or commit a binary asset.
  return (
    <svg viewBox="0 0 24 24" className="size-3.5" aria-hidden="true">
      <path fill="#EA4335" d="M12 11v3.4h5.4c-.2 1.3-1.5 3.7-5.4 3.7-3.2 0-5.9-2.7-5.9-5.9 0-3.3 2.7-5.9 5.9-5.9 1.8 0 3.1.8 3.8 1.5l2.6-2.5C16.7 3.7 14.6 2.8 12 2.8 6.9 2.8 2.8 6.9 2.8 12s4.1 9.2 9.2 9.2c5.3 0 8.8-3.7 8.8-9 0-.6-.1-1.1-.2-1.5H12z" />
    </svg>
  );
}
