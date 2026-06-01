// ─── Home — landing for signed-in creators ───────────────────────
//
// Hero question + lede + notebook prompt. Inspiration chips below.
// Recent work as a project gallery. The textarea + chips mirror the
// wizard's free-form path — submit shortcuts straight to /new with
// the prompt prefilled.
//
// Crystal: built over @zeroship/ui (PageFrame + Stack/Cluster/Grid
// layout primitives, Button, ToggleGroup, EmptyState) plus the
// already-migrated NotebookPrompt / ProjectCard. Bespoke editorial
// bits (hero type, accent underline, quote chips) live in the
// co-located Home.css reading --zs-* tokens. The public component
// interface, data hooks, routing, and test hooks are unchanged.

import { useMemo, useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Button,
  Cluster,
  Grid,
  Stack,
  ToggleGroup,
  Toggle,
  EmptyState,
} from "@zeroship/ui";
import { listProjects, unarchiveProject } from "../api";
import { PageFrame } from "../components/PageFrame";
import { NotebookPrompt, CmdEnterHint } from "../components/NotebookPrompt";
import { ProjectCard } from "../components/ProjectCard";
import "./Home.css";

const STORAGE_KEY = "zeroship_pending_prompt";

type GalleryFilter = "active" | "archived";

const INSPIRATION = [
  "A tip calculator that splits unevenly",
  "Markdown notes with a public share link",
  "A waiting list for my next book",
];

export function Home() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [prompt, setPrompt] = useState("");
  const [filter, setFilter] = useState<GalleryFilter>("active");

  const { data: apps, isLoading } = useQuery({
    queryKey: ["projects"],
    queryFn: () => listProjects(),
  });

  const restore = useMutation({
    mutationFn: async (id: string) => unarchiveProject({ appId: id }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["projects"] }),
  });

  // Split active vs archived once. The gallery below renders one or the
  // other based on the active filter segment.
  const { active, archived } = useMemo(() => {
    const a: typeof apps = [];
    const arc: typeof apps = [];
    for (const x of apps ?? []) {
      (x.archived ? arc : a).push(x);
    }
    return { active: a ?? [], archived: arc ?? [] };
  }, [apps]);

  const visible = filter === "active" ? active : archived;

  function submit(e?: FormEvent) {
    e?.preventDefault();
    const trimmed = prompt.trim();
    if (!trimmed) return;
    try { sessionStorage.setItem(STORAGE_KEY, trimmed); } catch {}
    navigate(`/new?prompt=${encodeURIComponent(trimmed)}`);
  }

  return (
    <PageFrame>
      <section className="zb-home__hero">
        <Stack gap={5}>
          <Stack gap={4}>
            <h1 className="zb-home__title">
              What will you{" "}
              <em className="zb-home__title-em">
                make
                <span aria-hidden className="zb-home__title-underline" />
              </em>
              ?
            </h1>
            <p className="zb-home__lede">
              Every project starts with a sentence. Describe it the way you'd
              describe it to a friend — the agent reads, drafts, builds and
              ships it as a real, live URL.{" "}
              <strong className="zb-home__lede-strong">Usually under a minute.</strong>
            </p>
          </Stack>

          <form onSubmit={submit}>
            <NotebookPrompt
              label="New project · draft"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onCmdEnter={() => submit()}
              placeholder="A recipe sharing space for my supper club — guests sign in, post photos, vote on who hosts next…"
              hint={<CmdEnterHint verb="to send · we'll keep your draft" />}
              action={
                <Button
                  type="submit"
                  variant="filled"
                  disabled={!prompt.trim()}
                  data-testid="home-submit"
                >
                  Begin
                </Button>
              }
              data-testid="home-prompt"
            />
          </form>

          <Cluster gap={2}>
            {INSPIRATION.map((s) => (
              <button
                key={s}
                type="button"
                onClick={() => setPrompt(s)}
                className="zb-home__chip"
              >
                <span className="zb-home__chip-quote">"</span>
                {s}
                <span className="zb-home__chip-quote">"</span>
              </button>
            ))}
          </Cluster>
        </Stack>
      </section>

      <section className="zb-home__gallery" data-testid="home-gallery">
        <Stack gap={4}>
          <Stack gap={3}>
            <Cluster justify="between" className="zb-home__gallery-head">
              <h2 className="zb-home__gallery-title">
                {filter === "archived" ? "Archived" : "Recent work"}
              </h2>
              <span className="zb-home__gallery-count">
                {visible.length} {visible.length === 1 ? "project" : "projects"}
              </span>
            </Cluster>
            <hr className="zb-home__divider" />
          </Stack>

          <Cluster justify="between" align="center">
            <ToggleGroup
              value={filter}
              onValueChange={(v) => { if (v) setFilter(v as GalleryFilter); }}
              size="sm"
              aria-label="Filter projects"
              data-testid="home-filters"
            >
              <Toggle value="active" data-testid="home-filter-active">
                Active
              </Toggle>
              <Toggle value="archived" data-testid="home-filter-archived">
                Archived
                {archived.length > 0 && (
                  <span className="zb-home__filter-count">{archived.length}</span>
                )}
              </Toggle>
            </ToggleGroup>
            <Link to="/templates" className="zb-home__templates-link">
              Browse templates →
            </Link>
          </Cluster>

          {isLoading ? (
            <p className="zb-home__loading">loading…</p>
          ) : visible.length === 0 ? (
            <EmptyState
              data-testid="home-empty"
              className="zb-home__empty"
              title={filter === "archived" ? "Nothing in the archive" : "No projects yet"}
              description={
                filter === "archived"
                  ? "Projects you tuck away will land here."
                  : "Start one above — describe it in a sentence."
              }
            />
          ) : (
            <Grid
              data-testid={filter === "archived" ? "home-archived-list" : "home-active-list"}
              minColWidth="16.25rem"
              gap={5}
            >
              {[...visible]
                .sort((a, b) => (b.updated_at ?? "").localeCompare(a.updated_at ?? ""))
                .map((app, i) => (
                  <div key={app.id} className="zb-home__tile">
                    <ProjectCard
                      app={app}
                      num={String(visible.length - i).padStart(2, "0")}
                    />
                    {filter === "archived" && (
                      <Button
                        type="button"
                        variant="gray"
                        size="small"
                        onClick={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          restore.mutate(app.id);
                        }}
                        data-testid={`home-restore:${app.id}`}
                        loading={restore.isPending && restore.variables === app.id}
                        className="zb-home__restore"
                      >
                        Restore
                      </Button>
                    )}
                  </div>
                ))}
            </Grid>
          )}
        </Stack>
      </section>
    </PageFrame>
  );
}
