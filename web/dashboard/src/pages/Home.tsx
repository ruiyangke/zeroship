// ─── Home — gallery + new-project prompt ────────────────────────
//
// Lands every signed-in creator. Top half: a big "what do you want
// to build?" textarea — first message becomes the project's first
// chat turn. Bottom half: cards for every existing project, sorted
// by last-updated.

import { useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useQuery, useMutation } from "@tanstack/react-query";
import { listApps, createApp, type AppRecord } from "../api";
import { TopBar } from "../workspace/components/TopBar";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Loader2, Sparkles, ArrowUpRight } from "lucide-react";

interface Props {
  onLogout?: () => void;
}

const PENDING_KEY = "zeroship_home_pending_prompt";

export function Home({ onLogout }: Props) {
  const [prompt, setPrompt] = useState("");
  const navigate = useNavigate();

  const { data: apps, isLoading } = useQuery({
    queryKey: ["apps"],
    queryFn: listApps,
  });

  const createMut = useMutation({
    mutationFn: async (firstPrompt: string) => {
      // Slug derived from the first words of the prompt — agent will
      // rename later if it picks something better.
      const slug = makeSlug(firstPrompt);
      const app = await createApp(slug, "free");
      return { app, firstPrompt };
    },
    onSuccess: ({ app, firstPrompt }) => {
      // Stash the prompt so the workspace's chat picks it up on mount.
      try { sessionStorage.setItem(PENDING_KEY, firstPrompt); } catch {}
      navigate(`/p/${app.id}/chat`);
    },
  });

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!prompt.trim() || createMut.isPending) return;
    createMut.mutate(prompt.trim());
  }

  return (
    <div className="h-screen flex flex-col">
      <TopBar onLogout={onLogout} />
      <main className="flex-1 overflow-auto" data-testid="home">
        <div className="max-w-4xl mx-auto px-6 py-12">
          {/* Hero: prompt */}
          <section className="mb-12">
            <h1 className="text-3xl font-bold tracking-tight mb-2">
              what do you want to build?
            </h1>
            <p className="text-sm text-muted-foreground mb-6">
              describe your app — a recipe sharing app, a tip calculator, a markdown editor —
              and the agent will scaffold it, run it, and surface the live preview.
            </p>
            <form onSubmit={submit} className="space-y-3">
              <Textarea
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) submit(e);
                }}
                placeholder="A recipe sharing app with user accounts and a paid tier…"
                rows={4}
                className="text-sm font-mono resize-none"
                disabled={createMut.isPending}
                data-testid="home-prompt"
              />
              <div className="flex items-center gap-3">
                <Button
                  type="submit" variant="primary"
                  disabled={!prompt.trim() || createMut.isPending}
                  className="h-9 px-4"
                  data-testid="home-submit"
                >
                  {createMut.isPending ? (
                    <Loader2 className="size-3 animate-spin mr-1.5" />
                  ) : (
                    <Sparkles className="size-3 mr-1.5" />
                  )}
                  {createMut.isPending ? "creating…" : "build it"}
                </Button>
                <span className="text-[11px] text-muted-foreground">
                  ⌘+enter to submit
                </span>
                {createMut.isError && (
                  <span className="text-xs text-destructive">
                    {createMut.error.message}
                  </span>
                )}
              </div>
            </form>
          </section>

          {/* Gallery */}
          <section data-testid="home-gallery">
            <div className="flex items-baseline justify-between mb-3">
              <h2 className="text-sm font-medium tracking-wider uppercase text-muted-foreground">
                projects
              </h2>
              <span className="text-xs text-muted-foreground">
                {apps?.length ?? 0} total
              </span>
            </div>

            {isLoading ? (
              <div className="text-xs text-muted-foreground">loading…</div>
            ) : !apps || apps.length === 0 ? (
              <div className="border border-dashed border-border text-xs text-muted-foreground p-8 text-center">
                no projects yet — start one above
              </div>
            ) : (
              <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3">
                {[...apps]
                  .sort((a, b) => (b.updated_at ?? "").localeCompare(a.updated_at ?? ""))
                  .map((app) => <ProjectCard key={app.id} app={app} />)}
              </div>
            )}
          </section>
        </div>
      </main>
    </div>
  );
}

function ProjectCard({ app }: { app: AppRecord }) {
  const live = !!app.deploy_hash;
  return (
    <Link
      to={`/p/${app.id}/chat`}
      data-testid={`home-card:${app.name}`}
      className="block border border-border bg-card hover:border-primary/40 hover:bg-white/[0.02] transition-colors p-4 group"
    >
      <div className="flex items-start justify-between mb-3">
        <span className="text-sm font-medium truncate flex-1">{app.name}</span>
        <ArrowUpRight className="size-3 text-muted-foreground opacity-0 group-hover:opacity-100 transition-opacity shrink-0 ml-2" />
      </div>
      <div className="flex items-center gap-2 text-[10px] uppercase tracking-wider">
        <span className="inline-flex items-center gap-1 text-muted-foreground">
          <span
            className={`size-1.5 rounded-full ${live ? "bg-emerald-500" : "bg-muted-foreground/40"}`}
          />
          {live ? "deployed" : "draft"}
        </span>
        <span className="text-muted-foreground/60">·</span>
        <span className="text-muted-foreground">{app.plan_id}</span>
      </div>
      <div className="text-[10px] text-muted-foreground/60 font-mono mt-2 truncate">
        {fmtDate(app.updated_at)}
      </div>
    </Link>
  );
}

function makeSlug(s: string): string {
  // First 3 words, sanitized. Falls back to a random suffix if the
  // result is empty — the platform itself dedupes by name on create.
  const words = s.toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .split(/\s+/)
    .slice(0, 3);
  const base = words.join("-").slice(0, 40);
  const suffix = Math.random().toString(36).slice(2, 6);
  return base ? `${base}-${suffix}` : `app-${suffix}`;
}

function fmtDate(s: string): string {
  try {
    const d = new Date(s);
    if (isNaN(d.getTime())) return s;
    const ms = Date.now() - d.getTime();
    const sec = Math.floor(ms / 1000);
    if (sec < 60) return "just now";
    if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
    if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
    return d.toISOString().slice(0, 10);
  } catch { return s; }
}

/** Helper for the workspace chat to consume the home-page prompt
 *  when it mounts a fresh project. Returns the prompt and clears it. */
export function consumePendingPrompt(): string | null {
  try {
    const v = sessionStorage.getItem(PENDING_KEY);
    if (v) sessionStorage.removeItem(PENDING_KEY);
    return v;
  } catch { return null; }
}
