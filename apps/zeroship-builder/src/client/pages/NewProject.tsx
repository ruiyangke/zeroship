// ─── NewProject — the 3-step wizard ─────────────────────────────
//
// Step 1: pick a template (or skip with the blank exit). Reads the
//         `?template=<slug>` query.
// Step 2: customise — pre-filled prompt, project name, URL preview.
// Step 3: confirm + Begin → calls createApp + stashes the prompt
//         for the workspace's chat to consume on mount.
//
// Reads home's pending prompt from sessionStorage if present, so
// "type-on-home → submit" lands here pre-filled.

import { useEffect, useMemo, useState, type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { useMutation } from "@tanstack/react-query";
import { createApp } from "../api";
import { PageFrame } from "../components/PageFrame";
import { NotebookPrompt } from "../components/NotebookPrompt";
import { StampButton } from "../components/StampButton";
import { GhostButton } from "../components/GhostButton";
import { TemplateCard } from "../components/TemplateCard";
import { FilterPill } from "../components/FilterPill";
import { TEMPLATES, TEMPLATE_CATEGORIES, getTemplate, type TemplateCategory } from "../lib/templates";
import { consumePendingPrompt } from "./Home";

type Step = 1 | 2 | 3;

const PENDING_PROMPT_KEY = "zeroship_pending_prompt";

export function NewProject() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const templateSlug = params.get("template");
  const homePrompt = useMemo(() => params.get("prompt") ?? consumePendingPrompt() ?? "", []);
  const template = getTemplate(templateSlug);

  // Step inference: have a template? skip step 1.
  const [step, setStep] = useState<Step>(templateSlug ? 2 : 1);

  const [filter, setFilter] = useState<"all" | TemplateCategory>("all");
  const visibleTemplates = filter === "all" ? TEMPLATES : TEMPLATES.filter((t) => t.category === filter);

  const [prompt, setPrompt] = useState(homePrompt || template?.defaultPrompt || "");
  const [name, setName] = useState(suggestName(template?.name ?? homePrompt));
  const [slug, setSlug] = useState(makeSlug(name));

  // Keep slug synced when name typing (only until user touches slug input)
  const [slugTouched, setSlugTouched] = useState(false);
  useEffect(() => {
    if (!slugTouched) setSlug(makeSlug(name));
  }, [name, slugTouched]);

  // Update prompt when template changes via search-param
  useEffect(() => {
    if (template) {
      setPrompt((prev) => prev || template.defaultPrompt);
      setName((prev) => prev || suggestName(template.name));
    }
  }, [template?.slug]);

  const create = useMutation({
    mutationFn: async () => {
      const app = await createApp(slug, "free");
      try { sessionStorage.setItem(PENDING_PROMPT_KEY, prompt); } catch {}
      return app;
    },
    onSuccess: (app) => navigate(`/p/${app.id}/preview`),
  });

  function pickTemplate(slug: string) {
    const t = getTemplate(slug);
    if (!t) return;
    params.set("template", slug);
    setParams(params);
    setPrompt(t.defaultPrompt);
    setName(suggestName(t.name));
    setStep(2);
  }

  function pickBlank() {
    params.delete("template");
    setParams(params);
    setStep(2);
  }

  function next(e?: FormEvent) {
    e?.preventDefault();
    if (step === 2) {
      if (!prompt.trim() || !name.trim() || !slug.trim()) return;
      setStep(3);
      return;
    }
    if (step === 3) {
      create.mutate();
      return;
    }
  }

  function back() {
    if (step === 3) setStep(2);
    else if (step === 2) setStep(1);
  }

  return (
    <PageFrame
      crumb={[{ label: "studio", to: "/" }, { label: "templates", to: "/templates" }, { label: "begin" }]}
      maxWidth={960}
      showMarginalia={false}
    >
      <Steps step={step} />

      {step === 1 && (
        <section className="reveal" data-testid="wiz-step-1">
          <h1 className="font-serif font-medium text-[44px] leading-[0.98] -tracking-[0.02em] mb-3">
            Pick a <em className="italic text-tomato">starting point</em>.
          </h1>
          <p className="font-serif text-[16px] text-ink-soft mb-6 max-w-[560px]">
            Use a template or start with a blank page. You'll edit the brief in the next step.
          </p>

          <div className="flex flex-wrap gap-2.5 mb-6">
            {TEMPLATE_CATEGORIES.map(({ key, label }) => (
              <FilterPill key={key} active={filter === key} onClick={() => setFilter(key as any)}>
                {label}
              </FilterPill>
            ))}
          </div>

          <div className="grid gap-5 mb-8" style={{ gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))" }}>
            {visibleTemplates.map((t) => (
              <div key={t.slug} onClick={(e) => { e.preventDefault(); pickTemplate(t.slug); }}>
                <TemplateCard template={t} />
              </div>
            ))}
          </div>

          <button
            type="button"
            onClick={pickBlank}
            className="font-serif italic text-[16px] text-ink border-b border-rule hover:text-tomato hover:border-tomato py-2 bg-transparent border-x-0 border-t-0 cursor-pointer"
            data-testid="wiz-blank"
          >
            Or describe your own →
          </button>
        </section>
      )}

      {step === 2 && (
        <section className="reveal" data-testid="wiz-step-2">
          <div className="bg-white border border-rule px-9 py-8 mb-5 shadow-[0_18px_24px_-16px_rgba(34,22,12,0.1)]">
            <h2 className="font-serif font-medium text-[26px] -tracking-[0.015em] mb-1.5">
              {template ? (
                <>{template.name} · <button type="button" onClick={() => setStep(1)} className="font-serif italic text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80">change</button></>
              ) : (
                <>Describe what you want to <em className="italic text-tomato">make</em>.</>
              )}
            </h2>
            <p className="font-serif text-[15px] text-ink-soft mb-5 leading-[1.55]">
              {template
                ? "Tell me anything different about yours. The agent will read this and adjust the design, copy, and behaviour."
                : "A sentence or two is plenty. The more specific, the better the first draft."}
            </p>

            <form onSubmit={next}>
              <NotebookPrompt
                label="Notes"
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                onCmdEnter={() => next()}
                rows={4}
                placeholder="A recipe sharing space for my supper club…"
                outerClassName="mb-5"
                data-testid="wiz-prompt"
              />

              <div className="grid gap-6 mb-3" style={{ gridTemplateColumns: "2fr 3fr" }}>
                <label className="block">
                  <span className="block label-uc mb-1">Project name</span>
                  <input
                    type="text"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    className="w-full px-3 py-2.5 border border-rule bg-white font-serif text-[16px] text-ink outline-none focus:border-ink"
                    data-testid="wiz-name"
                  />
                </label>
                <label className="block">
                  <span className="block label-uc mb-1">URL preview</span>
                  <input
                    type="text"
                    value={`${slug}.zeroship.app`}
                    onChange={(e) => {
                      setSlugTouched(true);
                      const v = e.target.value.replace(/\.zeroship\.app$/, "");
                      setSlug(makeSlug(v));
                    }}
                    className="w-full px-3 py-2.5 border border-rule bg-white font-mono text-[14px] text-ink outline-none focus:border-ink"
                    data-testid="wiz-slug"
                  />
                  <span className="block mt-1 font-serif italic text-[12px] text-pencil">
                    Must be unique. We'll suggest if taken.
                  </span>
                </label>
              </div>

              <div className="flex justify-between mt-4">
                <GhostButton onClick={back}>← Back</GhostButton>
                <StampButton type="submit" disabled={!prompt.trim() || !name.trim() || !slug.trim()} data-testid="wiz-next">
                  Next
                </StampButton>
              </div>
            </form>
          </div>
        </section>
      )}

      {step === 3 && (
        <section className="reveal" data-testid="wiz-step-3">
          <div className="bg-white border border-rule px-9 py-8 mb-5 shadow-[0_18px_24px_-16px_rgba(34,22,12,0.1)]">
            <h2 className="font-serif font-medium text-[26px] -tracking-[0.015em] mb-2">
              Ready when you are.
            </h2>

            <div className="border border-rule bg-paper-2 px-7 py-5 my-5">
              <div className="label-uc mb-3">Just before we begin</div>
              <ul className="m-0 p-0 list-none font-serif text-[16px] leading-[1.85]">
                <li className="before:content-['·'] before:text-tomato before:font-bold before:mr-2.5">
                  {template ? <em className="italic">{template.name}</em> : "A new project"} based on your notes
                </li>
                <li className="before:content-['·'] before:text-tomato before:font-bold before:mr-2.5">
                  Live at <code className="font-mono text-[14px] bg-paper border border-rule-2 px-1.5 py-px">{slug}.zeroship.app</code>
                </li>
                <li className="before:content-['·'] before:text-tomato before:font-bold before:mr-2.5">
                  <em className="italic">Free plan</em> — no payment until you choose to add one
                </li>
              </ul>
            </div>

            <p className="font-serif text-[15px] text-ink-soft mb-5 leading-[1.55]">
              Usually under a minute. You can iterate after — just talk to the agent about anything you want to change.
            </p>

            {create.isError && (
              <div className="mb-4 px-3 py-2 border border-tomato bg-tomato/10 text-tomato font-serif italic">
                {create.error.message}
              </div>
            )}

            <div className="flex justify-between">
              <GhostButton onClick={back}>← Back</GhostButton>
              <StampButton onClick={() => next()} loading={create.isPending} data-testid="wiz-begin">
                {create.isPending ? "Beginning…" : "Begin"}
              </StampButton>
            </div>
          </div>
        </section>
      )}
    </PageFrame>
  );
}

function Steps({ step }: { step: Step }) {
  const items: { n: Step; label: string }[] = [
    { n: 1, label: "step 1 · pick" },
    { n: 2, label: "step 2 · make it yours" },
    { n: 3, label: "step 3 · begin" },
  ];
  return (
    <div className="flex items-center gap-3.5 font-sans text-[10.5px] uppercase tracking-[0.2em] text-pencil mb-8">
      {items.map((it, i) => {
        const done = step > it.n;
        const active = step === it.n;
        return (
          <span key={it.n} className="flex items-center gap-2.5">
            <span
              className="inline-flex items-center gap-2"
              style={{
                color: done ? "var(--color-tomato)" : active ? "var(--color-ink)" : "var(--color-pencil)",
                fontWeight: active ? 600 : 400,
              }}
            >
              <span
                className="size-2.5 rounded-full"
                style={{
                  background: done ? "var(--color-tomato)" : active ? "var(--color-ink)" : "transparent",
                  border: "1px solid currentColor",
                }}
              />
              {it.label}
            </span>
            {i < items.length - 1 && <span className="block w-9 h-px bg-rule" />}
          </span>
        );
      })}
    </div>
  );
}

function suggestName(s: string | undefined): string {
  if (!s) return "New project";
  const words = s.replace(/[^\w\s'-]/g, "").trim().split(/\s+/).slice(0, 3);
  return words.length ? words.map((w) => w[0].toUpperCase() + w.slice(1)).join(" ") : "New project";
}

function makeSlug(s: string): string {
  const base = (s || "")
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .split(/\s+/)
    .slice(0, 3)
    .join("-")
    .slice(0, 40);
  if (base) return base;
  return "app-" + Math.random().toString(36).slice(2, 6);
}
