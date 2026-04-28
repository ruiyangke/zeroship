// ─── EnvTab — vars + secrets ────────────────────────────────────
// Plain config (vars) is plaintext + visible to the runtime.
// Secrets are encrypted at rest, the value is never returned —
// only the key list is shown.

import { useState, type FormEvent } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  listVars, setVar, deleteVar,
  listSecrets, setSecret, deleteSecret,
} from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Trash2, Loader2, Plus } from "lucide-react";

export function EnvTab() {
  const { appId } = useWorkspace();

  return (
    <div data-testid="env-tab" className="h-full overflow-auto p-6 max-w-3xl mx-auto space-y-6">
      <header>
        <h1 className="text-lg font-medium tracking-wider">// environment</h1>
        <p className="text-xs text-muted-foreground mt-1">
          vars are visible to the runtime as <code className="font-mono">env.NAME</code>;
          secrets are encrypted at rest and only readable inside your app.
        </p>
      </header>

      <VarsCard appId={appId} />
      <SecretsCard appId={appId} />
    </div>
  );
}

// ─── Plain vars ────────────────────────────────────────────────

function VarsCard({ appId }: { appId: string }) {
  const queryClient = useQueryClient();
  const [k, setK] = useState("");
  const [v, setV] = useState("");

  const { data, isLoading } = useQuery({
    queryKey: ["vars", appId],
    queryFn: () => listVars(appId),
  });

  const setMut = useMutation({
    mutationFn: ({ key, value }: { key: string; value: string }) => setVar(appId, key, value),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["vars", appId] });
      setK(""); setV("");
    },
  });

  const delMut = useMutation({
    mutationFn: (key: string) => deleteVar(appId, key),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["vars", appId] }),
  });

  function add(e: FormEvent) {
    e.preventDefault();
    if (!k.trim() || !v) return;
    setMut.mutate({ key: k.trim(), value: v });
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>vars · {data?.vars.length ?? 0}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <form onSubmit={add} className="grid grid-cols-[1fr_2fr_auto] gap-2 items-end">
          <div>
            <Label htmlFor="var-key">name</Label>
            <Input
              id="var-key" value={k} onChange={(e) => setK(e.target.value)}
              placeholder="API_BASE_URL" pattern="[A-Za-z_][A-Za-z0-9_]*"
              data-testid="env-var-key"
            />
          </div>
          <div>
            <Label htmlFor="var-val">value</Label>
            <Input
              id="var-val" value={v} onChange={(e) => setV(e.target.value)}
              placeholder="https://api.example.com" data-testid="env-var-value"
            />
          </div>
          <Button
            type="submit" variant="primary"
            disabled={!k.trim() || !v || setMut.isPending}
            data-testid="env-var-add"
          >
            {setMut.isPending ? <Loader2 className="size-3 animate-spin" /> : <Plus className="size-3" />}
            add
          </Button>
        </form>

        {isLoading ? (
          <div className="text-xs text-muted-foreground">loading…</div>
        ) : !data?.vars.length ? (
          <div className="text-xs text-muted-foreground italic">no vars yet</div>
        ) : (
          <ul className="divide-y divide-border border border-border">
            {data.vars.map((row) => (
              <li
                key={row.key}
                className="grid grid-cols-[1fr_2fr_auto] gap-2 items-center px-3 py-2 text-xs font-mono"
              >
                <span className="text-foreground truncate">{row.key}</span>
                <span className="text-muted-foreground truncate">{row.value}</span>
                <Button
                  type="button" variant="ghost"
                  className="h-7 w-7 p-0"
                  onClick={() => delMut.mutate(row.key)}
                  title="Delete"
                >
                  <Trash2 className="size-3 text-destructive" />
                </Button>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

// ─── Secrets ───────────────────────────────────────────────────

function SecretsCard({ appId }: { appId: string }) {
  const queryClient = useQueryClient();
  const [k, setK] = useState("");
  const [v, setV] = useState("");

  const { data, isLoading } = useQuery({
    queryKey: ["secrets", appId],
    queryFn: () => listSecrets(appId),
  });

  const setMut = useMutation({
    mutationFn: ({ key, value }: { key: string; value: string }) => setSecret(appId, key, value),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["secrets", appId] });
      setK(""); setV("");
    },
  });

  const delMut = useMutation({
    mutationFn: (key: string) => deleteSecret(appId, key),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["secrets", appId] }),
  });

  function add(e: FormEvent) {
    e.preventDefault();
    if (!k.trim() || !v) return;
    setMut.mutate({ key: k.trim(), value: v });
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>secrets · {data?.secrets.length ?? 0}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <form onSubmit={add} className="grid grid-cols-[1fr_2fr_auto] gap-2 items-end">
          <div>
            <Label htmlFor="sec-key">name</Label>
            <Input
              id="sec-key" value={k} onChange={(e) => setK(e.target.value)}
              placeholder="STRIPE_SECRET_KEY" pattern="[A-Za-z_][A-Za-z0-9_]*"
              data-testid="env-secret-key"
            />
          </div>
          <div>
            <Label htmlFor="sec-val">value</Label>
            <Input
              id="sec-val" type="password" value={v} onChange={(e) => setV(e.target.value)}
              placeholder="sk_live_…" data-testid="env-secret-value"
            />
          </div>
          <Button
            type="submit" variant="primary"
            disabled={!k.trim() || !v || setMut.isPending}
            data-testid="env-secret-add"
          >
            {setMut.isPending ? <Loader2 className="size-3 animate-spin" /> : <Plus className="size-3" />}
            add
          </Button>
        </form>

        {isLoading ? (
          <div className="text-xs text-muted-foreground">loading…</div>
        ) : !data?.secrets.length ? (
          <div className="text-xs text-muted-foreground italic">no secrets yet</div>
        ) : (
          <ul className="divide-y divide-border border border-border">
            {data.secrets.map((name) => (
              <li
                key={name}
                className="flex items-center gap-2 px-3 py-2 text-xs font-mono"
              >
                <span className="text-foreground flex-1 truncate">{name}</span>
                <Badge variant="muted">encrypted</Badge>
                <Button
                  type="button" variant="ghost"
                  className="h-7 w-7 p-0"
                  onClick={() => delMut.mutate(name)}
                  title="Delete"
                >
                  <Trash2 className="size-3 text-destructive" />
                </Button>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}
