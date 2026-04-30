// ─── Home — landing for signed-in creators ───────────────────────
//
// Hero question + lede + notebook prompt. Inspiration chips below.
// Recent work as letterhead cards. The textarea + chips mirror the
// wizard's free-form path — submit shortcuts straight to /new with
// the prompt prefilled.

import { useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { listApps } from "../api";
import { PageFrame } from "../components/PageFrame";
import { NotebookPrompt, CmdEnterHint } from "../components/NotebookPrompt";
import { StampButton } from "../components/StampButton";
import { ProjectCard } from "../components/ProjectCard";

const STORAGE_KEY = "zeroship_pending_prompt";

export function Home() {
  const navigate = useNavigate();
  const [prompt, setPrompt] = useState("");

  const { data: apps, isLoading } = useQuery({
    queryKey: ["apps"],
    queryFn: listApps,
  });

  function submit(e?: FormEvent) {
    e?.preventDefault();
    const trimmed = prompt.trim();
    if (!trimmed) return;
    try { sessionStorage.setItem(STORAGE_KEY, trimmed); } catch {}
    navigate(`/new?prompt=${encodeURIComponent(trimmed)}`);
  }

  return (
    <PageFrame>
      <section className="reveal">
        <h1
          className="font-serif font-medium leading-[0.98] -tracking-[0.02em] mb-5"
          style={{ fontSize: "clamp(48px, 6vw, 80px)", fontVariationSettings: '"opsz" 144' }}
        >
          What will you{" "}
          <em className="italic text-tomato relative inline-block">
            make
            <span
              aria-hidden
              className="absolute -bottom-0.5 left-0 right-0 h-1.5 bg-tomato opacity-20 rounded-sm"
              style={{ transform: "rotate(-1.5deg)" }}
            />
          </em>
          ?
        </h1>
        <p className="font-serif text-[18px] leading-[1.55] text-ink-soft max-w-[540px] mb-7">
          Every project starts with a sentence. Describe it the way you'd
          describe it to a friend — the agent reads, drafts, builds and
          ships it as a real, live URL.{" "}
          <strong className="text-ink font-medium">Usually under a minute.</strong>
        </p>

        <form onSubmit={submit}>
          <NotebookPrompt
            label="New project · draft"
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            onCmdEnter={() => submit()}
            placeholder="A recipe sharing space for my supper club — guests sign in, post photos, vote on who hosts next…"
            hint={<CmdEnterHint verb="to send · we'll keep your draft" />}
            action={
              <StampButton type="submit" disabled={!prompt.trim()} data-testid="home-submit">
                Begin
              </StampButton>
            }
            data-testid="home-prompt"
          />
        </form>

        <div className="flex flex-wrap gap-2 mt-3 mb-12">
          {[
            "A tip calculator that splits unevenly",
            "Markdown notes with a public share link",
            "A waiting list for my next book",
          ].map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => setPrompt(s)}
              className="bg-transparent border border-rule rounded-full px-3 py-1.5 font-serif italic text-[13px] text-ink hover:border-ink hover:bg-paper-2 transition-colors cursor-pointer"
            >
              <span className="text-pencil">"</span>{s}<span className="text-pencil">"</span>
            </button>
          ))}
        </div>
      </section>

      <section className="reveal d-1" data-testid="home-gallery">
        <div className="flex items-baseline justify-between mb-3">
          <h2 className="font-serif italic font-medium text-[26px] -tracking-[0.01em] m-0">Recent work</h2>
          <span className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-pencil">
            {apps?.length ?? 0} projects
          </span>
        </div>
        <hr className="hairline mb-5" />

        <div className="flex items-baseline justify-between mb-5">
          <Link to="/templates" className="font-serif italic text-[14px] text-tomato hover:opacity-80" style={{ textDecoration: "none" }}>
            Browse templates →
          </Link>
        </div>

        {isLoading ? (
          <div className="font-serif italic text-ink-soft text-[14px]">loading…</div>
        ) : !apps || apps.length === 0 ? (
          <div className="border border-dashed border-rule p-8 text-center">
            <p className="font-serif italic text-ink-soft">No projects yet — start one above.</p>
          </div>
        ) : (
          <div className="grid gap-5" style={{ gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))" }}>
            {[...apps]
              .sort((a, b) => (b.updated_at ?? "").localeCompare(a.updated_at ?? ""))
              .map((app, i) => (
                <ProjectCard
                  key={app.id}
                  app={app}
                  num={String(apps.length - i).padStart(2, "0")}
                />
              ))}
          </div>
        )}
      </section>
    </PageFrame>
  );
}

/** Used by /new to consume the home-page prompt when navigating. */
export function consumePendingPrompt(): string | null {
  try {
    const v = sessionStorage.getItem(STORAGE_KEY);
    if (v) sessionStorage.removeItem(STORAGE_KEY);
    return v;
  } catch { return null; }
}
