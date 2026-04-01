import { useState, type FormEvent } from "react";
import { useMutation } from "@tanstack/react-query";
import { getHealth } from "../api";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";

interface LoginProps {
  onLogin: () => void;
}

export default function Login({ onLogin }: LoginProps) {
  const [key, setKey] = useState("");

  const loginMutation = useMutation({
    mutationFn: async (masterKey: string) => {
      localStorage.setItem("appbase_key", masterKey.trim());
      try {
        await getHealth();
      } catch {
        localStorage.removeItem("appbase_key");
        throw new Error(
          "connection failed -- check your key and that the server is running on :3333"
        );
      }
    },
    onSuccess: () => onLogin(),
  });

  function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!key.trim()) return;
    loginMutation.mutate(key);
  }

  return (
    <div className="flex items-center justify-center h-screen p-5">
      <div className="w-[400px] max-w-full">
        <h1 className="text-sm font-bold tracking-[0.15em] uppercase text-primary mb-1">
          appbase
        </h1>
        <div className="text-xs text-muted-foreground mb-6">
          enter master key to continue
        </div>
        <Card>
          <CardContent>
            <form onSubmit={handleSubmit}>
              <div className="mb-4">
                <Label htmlFor="master-key">master key</Label>
                <Input
                  id="master-key"
                  type="password"
                  placeholder="sk_..."
                  value={key}
                  onChange={(e) => setKey(e.target.value)}
                  autoFocus
                />
              </div>
              <Button
                type="submit"
                variant="primary"
                className="w-full"
                disabled={loginMutation.isPending || !key.trim()}
              >
                {loginMutation.isPending ? "connecting..." : "authenticate"}
              </Button>
              {loginMutation.isError && (
                <div className="text-xs text-destructive mt-2">
                  {loginMutation.error.message}
                </div>
              )}
            </form>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
