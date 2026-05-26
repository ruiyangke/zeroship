// ─── PlanCanvas — PM agent's home (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.8) ───────────────────
//
// Three stacked sections inside one scrolling canvas:
//   1. Issues — status-grouped list (Open / In Progress / Done) with
//      a "+ New issue" modal at the top-right.
//   2. Roadmap — milestone strip with progress derived from issue
//      counts. Tomato accent for the current milestone.
//   3. Deployments — single-row "current deploy" V1; reads
//      `app.deploy_hash` + `app.updated_at`. Multi-row history lands
//      with the current in-memory implementation.
//
// All backend writes go through the in-memory stub in
// `src/server/agents.ts` — see its header for the V1 caveat.

import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  addIssue,
  listIssues,
  pmDigest,
  type AppRecord,
  type Issue,
  type IssueStatus,
  type PMDigest,
} from "../../api";
import { Modal } from "../../components/Modal";
import { StampButton } from "../../components/StampButton";
import { GhostButton } from "../../components/GhostButton";

export interface PlanCanvasProps {
  appId: string;
  app?: AppRecord;
}

export function PlanCanvas({ appId, app }: PlanCanvasProps) {
  return (
    <div data-testid="plan-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[860px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <DigestSection appId={appId} />
        <div className="h-12" />
        <IssuesSection appId={appId} />
        <div className="h-12" />
        <RoadmapSection appId={appId} />
        <div className="h-12" />
        <DeploymentsSection app={app} />
      </div>
    </div>
  );
}

// ─── 1pre · PM digest panel ─────────────────────────────────────
//
// Spec §13: PM agent has a digest mode that produces a 1-3 sentence
// project narrative + 1-3 ranked next moves. The scheduled-worker
// shape (cron) is not wired yet, but the proc itself works on
// demand — this surfaces a button so creators can pull the digest
// without waiting for the cron to land. The panel renders the
// last-fetched digest inline so a refresh (or revisit) keeps the
// signal visible.

function DigestSection({ appId }: { appId: string }) {
  const [digest, setDigest] = useState<PMDigest | null>(null);
  const run = useMutation({
    mutationFn: async () => pmDigest({ appId }),
    onSuccess: (d) => setDigest(d),
  });
  return (
    <section data-testid="plan-digest-section">
      <header className="flex items-baseline justify-between mb-4">
        <div>
          <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
            PM digest
          </h2>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            What shipped, what's next. Run this whenever — the PM agent
            reads the project and produces a short narrative.
          </p>
        </div>
        <GhostButton
          onClick={() => run.mutate()}
          disabled={run.isPending}
          data-testid="plan-run-digest"
        >
          {run.isPending ? "running…" : digest ? "Run again" : "Run digest"}
        </GhostButton>
      </header>
      {run.error && (
        <div
          data-testid="plan-digest-error"
          className="border border-tomato/40 bg-tomato/5 px-4 py-3 font-serif italic text-[13.5px] text-tomato"
        >
          {(run.error as Error).message || "Digest failed."}
        </div>
      )}
      {!digest && !run.isPending && !run.error && (
        <div
          data-testid="plan-digest-empty"
          className="border border-dashed border-rule bg-paper-2/40 py-6 text-center font-serif italic text-[13.5px] text-pencil"
        >
          No digest yet — run one to get a snapshot of project momentum.
        </div>
      )}
      {digest && (
        <div
          data-testid="plan-digest-result"
          className="border border-rule bg-paper-2/40 px-5 py-4"
        >
          <p className="font-serif text-[14.5px] text-ink leading-[1.6] m-0 mb-3">
            {digest.summary}
          </p>
          <div className="label-uc mb-2">Recommendations</div>
          <ol className="list-decimal pl-5 m-0 space-y-1.5">
            {digest.recommendations.map((r, i) => (
              <li
                key={i}
                data-testid="plan-digest-rec"
                className="font-serif text-[13.5px] text-ink leading-[1.55]"
              >
                <span className="font-medium">{r.title}</span>
                {r.urgency && (
                  <span className="ml-2 font-sans text-[10px] uppercase tracking-[0.18em] text-ink-soft border border-rule rounded-full px-2 py-0.5">
                    {r.urgency}
                  </span>
                )}
                <div className="font-serif italic text-[13px] text-ink-soft mt-0.5">
                  {r.why}
                </div>
              </li>
            ))}
          </ol>
        </div>
      )}
    </section>
  );
}

// ─── 1a · Issues ────────────────────────────────────────────────

function IssuesSection({ appId }: { appId: string }) {
  const qc = useQueryClient();
  const { data, isLoading } = useQuery({
    queryKey: ["plan-issues", appId],
    queryFn: () => listIssues({ appId }),
    retry: false,
  });
  const [composeOpen, setComposeOpen] = useState(false);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const create = useMutation({
    mutationFn: async (input: { title: string; description: string }) =>
      addIssue({ appId, ...input }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["plan-issues", appId] });
      qc.invalidateQueries({ queryKey: ["plan-roadmap", appId] });
      setComposeOpen(false);
    },
  });

  const groups = useMemo(() => groupByStatus(data?.issues ?? []), [data]);

  return (
    <section data-testid="plan-issues-section">
      <header className="flex items-baseline justify-between mb-4">
        <div>
          <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
            Issues
          </h2>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            What's open, what's in flight, what shipped. The PM agent and
            the Critic file these — you can too.
          </p>
        </div>
        <button
          type="button"
          onClick={() => setComposeOpen(true)}
          data-testid="plan-new-issue"
          className="font-serif italic text-[14px] text-tomato bg-transparent border-0 cursor-pointer hover:opacity-80"
        >
          + New issue
        </button>
      </header>

      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}

      {!isLoading && (data?.issues.length ?? 0) === 0 && (
        <div
          data-testid="plan-issues-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          Builder hasn't filed any issues yet — neither has the Critic.
          File the first one above.
        </div>
      )}

      {(["open", "in_progress", "done"] as const).map((status) => {
        const list = groups[status];
        if (list.length === 0) return null;
        return (
          <div key={status} className="mb-5">
            <div className="label-uc mb-1.5">
              {prettyStatus(status)} <span className="text-pencil">({list.length})</span>
            </div>
            <div>
              {list.map((iss) => (
                <IssueRow
                  key={iss.id}
                  issue={iss}
                  expanded={expandedId === iss.id}
                  onToggle={() =>
                    setExpandedId(expandedId === iss.id ? null : iss.id)
                  }
                />
              ))}
            </div>
          </div>
        );
      })}

      <NewIssueModal
        open={composeOpen}
        onClose={() => !create.isPending && setComposeOpen(false)}
        onSubmit={(input) => create.mutate(input)}
        submitting={create.isPending}
      />
    </section>
  );
}

function IssueRow({
  issue,
  expanded,
  onToggle,
}: {
  issue: Issue;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <div
      data-testid="plan-issue-row"
      className="border-b border-rule-2"
    >
      <button
        type="button"
        onClick={onToggle}
        className="w-full grid items-center gap-4 py-3 bg-transparent border-0 cursor-pointer text-left hover:bg-paper-2"
        style={{ gridTemplateColumns: "16px 1fr auto auto" }}
      >
        <StatusDot status={issue.status} />
        <span className="font-serif text-[14.5px] text-ink truncate">
          {issue.title}
        </span>
        {issue.assignee ? (
          <span className="font-sans text-[10px] uppercase tracking-[0.18em] text-ink-soft border border-rule rounded-full px-2 py-0.5">
            {issue.assignee}
          </span>
        ) : (
          <span />
        )}
        <span className="font-serif italic text-[12px] text-pencil">
          {relativeTime(issue.updated_at)}
        </span>
      </button>
      {expanded && (
        <div
          data-testid="plan-issue-detail"
          className="px-6 pb-4 pt-1 grid gap-3 bg-paper-2/40 border-t border-rule-2"
        >
          <p className="font-serif text-[13.5px] text-ink leading-[1.55] m-0">
            {issue.description || (
              <em className="text-pencil">No description.</em>
            )}
          </p>
          {issue.comments.length > 0 && (
            <div>
              <div className="label-uc mb-1">Comments</div>
              {issue.comments.map((c, i) => (
                <div key={i} className="border-l-2 border-rule pl-3 py-1">
                  <div className="font-sans text-[10px] uppercase tracking-[0.18em] text-ink-soft">
                    {c.author} · {relativeTime(c.at)}
                  </div>
                  <div className="font-serif text-[13.5px] text-ink leading-[1.5]">
                    {c.body}
                  </div>
                </div>
              ))}
            </div>
          )}
          <div className="font-serif italic text-[12px] text-pencil">
            Filed by {issue.source} · {relativeTime(issue.created_at)}
          </div>
        </div>
      )}
    </div>
  );
}

function StatusDot({ status }: { status: IssueStatus }) {
  const colour =
    status === "open"
      ? "bg-tomato"
      : status === "in_progress"
        ? "bg-ivy"
        : "bg-pencil";
  const ring = status === "in_progress" ? "ring-2 ring-ivy/30" : "";
  return (
    <span
      aria-hidden="true"
      className={`size-2.5 rounded-full ${colour} ${ring} inline-block`}
    />
  );
}

function NewIssueModal({
  open,
  onClose,
  onSubmit,
  submitting,
}: {
  open: boolean;
  onClose: () => void;
  onSubmit: (input: { title: string; description: string }) => void;
  submitting: boolean;
}) {
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const canSave = title.trim().length > 0;

  function reset() {
    setTitle("");
    setDescription("");
  }

  return (
    <Modal
      open={open}
      onClose={() => {
        if (submitting) return;
        reset();
        onClose();
      }}
      title="New issue"
    >
      <div data-testid="plan-new-issue-modal">
        <label className="block mb-3">
          <span className="block label-uc mb-1">Title</span>
          <input
            autoFocus
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="What's wrong, or what should change?"
            data-testid="plan-new-issue-title"
            className="w-full px-3 py-2 border border-rule bg-white font-serif text-[14px] outline-none focus:border-ink"
          />
        </label>
        <label className="block mb-4">
          <span className="block label-uc mb-1">Description</span>
          <textarea
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            rows={4}
            placeholder="More detail. Steps to reproduce, expected vs actual…"
            data-testid="plan-new-issue-description"
            className="w-full px-3 py-2 border border-rule bg-white font-serif text-[14px] outline-none focus:border-ink resize-y"
          />
        </label>
        <div className="flex justify-end gap-3">
          <button
            type="button"
            disabled={submitting}
            onClick={() => {
              reset();
              onClose();
            }}
            className="font-serif italic text-[14px] text-pencil bg-transparent border-0 cursor-pointer disabled:opacity-50"
          >
            cancel
          </button>
          <StampButton
            disabled={!canSave}
            loading={submitting}
            onClick={() => {
              if (!canSave) return;
              onSubmit({ title: title.trim(), description: description.trim() });
              reset();
            }}
            data-testid="plan-new-issue-submit"
          >
            File issue
          </StampButton>
        </div>
      </div>
    </Modal>
  );
}

// ─── 1b · Roadmap ───────────────────────────────────────────────

interface Milestone {
  id: string;
  label: string;
  caption: string;
}

const MILESTONES: ReadonlyArray<Milestone> = [
  { id: "v0.1", label: "v0.1", caption: "first ship" },
  { id: "v0.2", label: "v0.2", caption: "polish + paid plans" },
  { id: "v0.3", label: "v0.3", caption: "scale + skills" },
];

function RoadmapSection({ appId }: { appId: string }) {
  const { data } = useQuery({
    queryKey: ["plan-roadmap", appId],
    queryFn: () => listIssues({ appId }),
    retry: false,
  });
  const issues = data?.issues ?? [];
  const total = issues.length;
  const done = issues.filter((i) => i.status === "done").length;

  return (
    <section data-testid="plan-roadmap-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Roadmap
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          A milestone is a promise to ship. Drag issues onto one when
          you're ready to commit.
        </p>
      </header>
      <div className="grid gap-4 grid-cols-1 sm:grid-cols-3">
        {MILESTONES.map((m, i) => {
          const current = i === 0;
          // V1: we attribute every issue to v0.1 (the only milestone
          // we have data for). v0.2 / v0.3 stay empty until a real
          // milestones table is not wired yet.
          const milestoneTotal = current ? total : 0;
          const milestoneDone = current ? done : 0;
          const ratio =
            milestoneTotal === 0 ? 0 : milestoneDone / milestoneTotal;
          return (
            <div
              key={m.id}
              data-testid={`plan-milestone:${m.id}`}
              className={
                "border bg-paper-2/40 px-4 py-3 " +
                (current ? "border-tomato" : "border-rule-2")
              }
            >
              <div className="flex items-baseline justify-between mb-1">
                <span
                  className={
                    "font-serif italic font-medium text-[18px] " +
                    (current ? "text-tomato" : "text-ink")
                  }
                >
                  {m.label}
                </span>
                <span className="font-mono text-[11px] text-ink-soft">
                  {milestoneDone}/{milestoneTotal}
                </span>
              </div>
              <div className="font-serif text-[12.5px] text-ink-soft mb-2">
                {m.caption}
              </div>
              <div className="h-1 bg-rule-2 overflow-hidden">
                <div
                  className={current ? "h-full bg-tomato" : "h-full bg-pencil"}
                  style={{ width: `${Math.round(ratio * 100)}%` }}
                />
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}

// ─── 1c · Deployments ───────────────────────────────────────────

function DeploymentsSection({ app }: { app?: AppRecord }) {
  const hasDeploy = !!app?.deploy_hash;
  return (
    <section data-testid="plan-deployments-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Deployments
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          Each ship is a labelled point in time. V1 shows the current
          deploy only — full history is on the way.
        </p>
      </header>
      {!hasDeploy && (
        <div
          data-testid="plan-deployments-empty"
          className="font-serif italic text-pencil py-8 text-center border border-dashed border-rule"
        >
          Not deployed yet — once Builder ships, the current pin lands here.
        </div>
      )}
      {hasDeploy && (
        <div
          data-testid="plan-deployments-current"
          className="grid items-center gap-4 px-4 py-3 border border-rule-2 bg-paper-2/40"
          style={{ gridTemplateColumns: "auto 1fr auto auto" }}
        >
          <span className="size-2.5 rounded-full bg-ivy inline-block" />
          <div>
            <div className="font-serif italic font-medium text-[16px] text-ink">
              current deploy
            </div>
            <div className="font-mono text-[11px] text-pencil">
              {(app?.deploy_hash ?? "").slice(0, 12)}
            </div>
          </div>
          <span className="font-sans text-[10px] uppercase tracking-[0.18em] text-ivy border border-ivy/40 rounded-full px-2 py-0.5">
            live
          </span>
          <span className="font-serif italic text-[12px] text-pencil">
            {app ? relativeTime(app.updated_at) : "—"}
          </span>
        </div>
      )}
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Full deployment history is not wired yet.
      </div>
    </section>
  );
}

// ─── helpers ────────────────────────────────────────────────────

function groupByStatus(
  issues: Issue[],
): Record<IssueStatus, Issue[]> {
  const groups: Record<IssueStatus, Issue[]> = {
    open: [],
    in_progress: [],
    done: [],
  };
  for (const i of issues) groups[i.status].push(i);
  return groups;
}

function prettyStatus(s: IssueStatus): string {
  if (s === "open") return "Open";
  if (s === "in_progress") return "In progress";
  return "Done";
}

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "—";
  const diff = Date.now() - t;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}
