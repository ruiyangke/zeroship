import { useEffect, useState } from "react";
import { Banner, Button, Card, Cluster, Input, Stack } from "@zeroship/ui";
import { KindBadge, PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "../components/Badges";
import { currentUser, getIssue, getProduct, listAttachments, listProducts, updateIssue } from "../api";
import { ErrorState, Loading } from "../components/StateViews";
import { isUnauthenticated } from "../components/rpc";
import { AttachmentsPanel } from "../components/issue-detail/AttachmentsPanel";
import { CcPanel } from "../components/issue-detail/CcPanel";
import { CommentsPanel } from "../components/issue-detail/CommentsPanel";
import { FieldsPanel } from "../components/issue-detail/FieldsPanel";
import { FlagsPanel } from "../components/issue-detail/FlagsPanel";
import { SecurityPanel } from "../components/issue-detail/SecurityPanel";
import { SeeAlsoPanel } from "../components/issue-detail/SeeAlsoPanel";
import { VotesPanel } from "../components/issue-detail/VotesPanel";
import { HistoryPanel } from "../components/issue-detail/HistoryPanel";
import { DependenciesPanel, DuplicatesPanel } from "../components/issue-detail/RelationsPanel";
import { KeywordsPanel } from "../components/issue-detail/KeywordsPanel";
import { errorMessage, toPromise, useAsync } from "../components/rpc";
import type { ProductDetail } from "../components/types";

type Tab = "details" | "history";

/**
 * Editing the issue's title, where the title is.
 *
 * This used to be "Edit summary" at the top of the metadata rail: a control
 * several hundred pixels from the words it changes, in the column reserved for
 * facts ABOUT the issue. A title is not metadata about itself, and an action
 * belongs beside its object.
 */
function TitleEditor({
  issue,
  onDone,
}: {
  issue: { id: string; summary: string };
  onDone: (changed: boolean) => void;
}) {
  const [draft, setDraft] = useState(issue.summary);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    const summary = draft.trim();
    if (!summary) return;
    setBusy(true);
    setError(null);
    try {
      await toPromise(updateIssue({ id: issue.id, changes: { summary } }));
      onDone(true);
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <div className="issue-title-editor">
      <Input
        aria-label="Summary"
        value={draft}
        disabled={busy}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            void save();
          }
          // Escape abandons, and the draft dies with the component.
          if (event.key === "Escape") onDone(false);
        }}
      />
      <Cluster gap={2} align="center">
        <Button variant="filled" size="small" disabled={busy || !draft.trim()} onClick={() => void save()}>
          Save
        </Button>
        <Button variant="plain" size="small" disabled={busy} onClick={() => onDone(false)}>
          Cancel
        </Button>
      </Cluster>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

export function IssueDetailPage({
  id,
  commentNumber,
}: {
  id: string;
  /** From #/issues/<id>/c/<n> -- scroll to that comment once it exists. */
  commentNumber?: number;
}) {
  const { state, reload } = useAsync(() => getIssue({ id }), [id]);
  const [tab, setTab] = useState<Tab>("details");

  // Land on the comment a permalink names.
  //
  // Waits for the element rather than scrolling on mount: the thread arrives
  // over RPC, so at mount there is nothing to scroll to and a naive
  // scrollIntoView would silently do nothing -- which is what a permalink
  // that "works" but goes nowhere looks like.
  useEffect(() => {
    if (!commentNumber) return;
    let cancelled = false;
    let tries = 0;
    const find = () => {
      if (cancelled) return;
      const el = document.getElementById(`comment-${commentNumber}`);
      if (el) {
        // scrollIntoView walks up to the nearest scrollable ancestor, which
        // here is the shell's main rather than the document.
        el.scrollIntoView({ block: "center" });
        el.classList.add("is-linked");
        return;
      }
      if (tries++ < 40) window.setTimeout(find, 100);
    };
    find();
    return () => {
      cancelled = true;
    };
  }, [commentNumber, id]);
  // CONTROLLED, because <details open> is not. Anything inside the fold that
  // reloads the issue -- voting, setting a flag, restricting it -- re-renders
  // this subtree, and an uncontrolled element comes back closed. The fold
  // would snap shut under someone in the middle of using it, which is both a
  // real defect and why the specs saw the panel become "not stable, then not
  // visible" while clicking.
  const [moreOpen, setMoreOpen] = useState(false);
  const [editingTitle, setEditingTitle] = useState(false);
  const [productDetail, setProductDetail] = useState<ProductDetail | null>(null);
  const productsQ = useAsync(() => listProducts({}), []);
  // Issues are public, so unlike the dashboard this page stays READABLE without
  // an identity -- what it must not do is offer controls that cannot work.
  const { state: userState } = useAsync(() => currentUser({}), []);
  // The page owns the file list because two children render it: the thread
  // shows each comment's files, and the roll-up shows every file on the issue.
  const attachmentsQ = useAsync(() => listAttachments({ issueId: id }), [id]);
  const signedOut = userState.status === "error" && isUnauthenticated(userState.error);

  const productId = state.status === "ready" ? state.data.product?.id ?? null : null;

  useEffect(() => {
    setProductDetail(null);
    if (!productId) return;
    let cancelled = false;
    toPromise(getProduct({ id: productId }))
      .then((detail) => {
        if (!cancelled) setProductDetail(detail);
      })
      .catch(() => {
        if (!cancelled) setProductDetail(null);
      });
    return () => {
      cancelled = true;
    };
  }, [productId]);

  if (state.status === "loading") return <Loading label="Loading issue..." />;
  if (state.status === "error") return <ErrorState error={state.error} onRetry={reload} />;

  const { data: detail } = state;
  if (!detail.product || !detail.component) {
    return (
      <ErrorState
        error={new Error("This issue references a product or component that no longer resolves.")}
      />
    );
  }
  const product = detail.product;
  const component = detail.component;

  // Everything the log might name by id. Built from what the page already
  // has, so the history tab costs no extra request: the issue's own product and
  // component, the people issues.get resolved, and the product structure the
  // fields panel loaded.
  const historyLabels: Record<string, string> = {
    [product.id]: product.name,
    [component.id]: component.name,
    ...Object.fromEntries(
      Object.values(detail.people ?? {}).map((person) => [person.id, person.name ?? person.handle]),
    ),
    ...Object.fromEntries((productDetail?.versions ?? []).map((v) => [v.id, v.name])),
    ...Object.fromEntries((productDetail?.milestones ?? []).map((m) => [m.id, m.name])),
    ...Object.fromEntries((productDetail?.components ?? []).map((c) => [c.id, c.name])),
    // Issues named by the log: dependsOn / blocks / duplicateOfId store another
    // issue's typed id as the value, so without these the history reads "set
    // Depends On to issue_0346W0Ole6amXDN9RKvzW6".
    ...Object.fromEntries(
      Object.entries(detail.issueRefs ?? {}).map(([id, ref]) => [id, ref.label]),
    ),
  };

  return (
    <div className="page issue-detail-page">
      {/* The key, the title and the state, in that reading order.
          These used to share one line: a mono key, a long summary and four
          badges all at the same height, so a title that ran long squeezed the
          badges and the eye had no obvious place to start. The key is now a
          quiet line above, the summary owns its own line at heading size, and
          the state sits under it where it reads as a caption about the issue
          rather than as more title. */}
      <div className="page-head issue-head">
        <span className="issue-id">
          {detail.product ? `${detail.product.key}-${detail.issue.number}` : detail.issue.id}
        </span>
        {editingTitle ? (
          <TitleEditor
            issue={detail.issue}
            onDone={(next) => {
              setEditingTitle(false);
              if (next) reload();
            }}
          />
        ) : (
          <Cluster gap={2} align="center" className="issue-title-row">
            <h1 className="issue-title">{detail.issue.summary}</h1>
            {signedOut ? null : (
              <Button
                variant="plain"
                size="small"
                className="issue-title-edit"
                aria-label="Edit summary"
                onClick={() => setEditingTitle(true)}
              >
                Edit
              </Button>
            )}
          </Cluster>
        )}
        <Cluster gap={2} align="center" className="issue-state">
          <StatusBadge status={detail.issue.status} />
          <ResolutionBadge resolution={detail.issue.resolution ?? null} />
          {/* Before severity, because it qualifies it: "blocker" answers how
              bad, and only kind answers how bad AT WHAT. Without this the top
              of a feature request and the top of a crash report were the same
              three badges. */}
          <KindBadge kind={detail.issue.kind} />
          <SeverityBadge severity={detail.issue.severity} />
          <PriorityBadge priority={detail.issue.priority} />
          <span className="issue-head-meta">
            {detail.product ? detail.product.name : null}
            {detail.component ? ` / ${detail.component.name}` : null}
          </span>
        </Cluster>
      </div>

      {/* Said once, at the top, rather than discovered one 401 at a time.
          A signed-out visitor could read this page and still be offered a
          comment box, an Edit button on every comment and three editable
          selects -- every one of which fails on use. The issue stays readable
          because issues are public; the controls that cannot work go away. */}
      {signedOut ? (
        <Banner intent="info" title="You are not signed in">
          This issue is public, so you can read it. Sign in to comment or change any
          of its fields.
        </Banner>
      ) : null}
      <div className="tabs">
        <button
          type="button"
          className={tab === "details" ? "active" : undefined}
          onClick={() => setTab("details")}
        >
          Details
        </button>
        <button
          type="button"
          className={tab === "history" ? "active" : undefined}
          onClick={() => setTab("history")}
        >
          History ({detail.activities.length})
        </button>
      </div>

      {tab === "history" ? (
        <HistoryPanel activities={detail.activities} labels={historyLabels} people={detail.people} />
      ) : (
        <div className="issue-detail-grid">
          {/* The conversation IS the issue. It used to sit under a screen of
              editable fields -- summary, status, severity, priority,
              assignee, product, component, version, whiteboard, OS, platform,
              URL -- so the description, which is what the issue actually says,
              started below the fold. Fields are metadata and metadata goes in
              the rail. */}
          <div className="issue-detail-main">
            <CommentsPanel
              issueId={id}
              readOnly={signedOut}
              activities={detail.activities}
              people={detail.people}
              labels={historyLabels}
              attachments={attachmentsQ.state}
              onAttachmentsChanged={attachmentsQ.reload}
            />
            {/* Everything below is about the issue WITHOUT being metadata about
                it: files, links to other issues, who is watching, what is
                pending review. They lived in the rail, where measurement put
                them at 1856px stacked inside a 352px column -- a column whose
                whole job is to stay glanceable. Here they get the main
                column's width and pair up two-across instead of forming one
                long ribbon. */}
            {/* Same disabling wrapper as the rail, for the same reason: these
                panels each carry an Add or Upload form that answers 401
                without a session. A fieldset covers all nine and any tenth,
                where nine readOnly props would each be a thing to remember.
                Their CONTENT still renders -- attachments, watchers and
                linked issues are readable facts about a public issue. */}
            <fieldset className="rail-fields issue-detail-extras" disabled={signedOut}>
              <AttachmentsPanel state={attachmentsQ.state} reload={attachmentsQ.reload} />
            </fieldset>

            {/* The long tail, folded.
                Six of these nine panels report ABSENCE on a typical issue -- no
                duplicates, no linked reports, no keywords, no flags, no votes,
                no group -- and each one drew a full card to say so. That is
                four hundred pixels of grid carrying no information, directly
                under the conversation. Files, CC and dependencies stay out
                because they are the ones that usually have something in them;
                the rest are one click away, and the summary says so rather
                than making you open it to find out. */}
            {/* A button and a conditional, NOT <details>.
                React manages the open attribute while the browser also owns
                it, and the two disagreed: any re-render inside the fold reset
                the element, so a panel would go "not stable, then not
                visible" under a click. One owner of the state removes the
                argument entirely. */}
            <div className="issue-detail-more">
              <Button
                variant="plain"
                size="small"
                aria-expanded={moreOpen}
                onClick={() => setMoreOpen((open) => !open)}
              >
                {moreOpen ? "Hide" : "Show"} flags, votes and security
              </Button>
              {moreOpen ? (
              <fieldset className="rail-fields issue-detail-extras" disabled={signedOut}>
              {/* No `activities` prop: the panel reads real flags from
                  flags.list instead of replaying the issue's history. */}
              <FlagsPanel issueId={id} flagTypes={productDetail?.flagTypes ?? null} onChanged={reload} />
              <VotesPanel
                issueId={id}
                voteCount={detail.issue.voteCount}
                maxVotesPerIssue={productDetail?.product.maxVotesPerIssue ?? 0}
                votingEnabled={(productDetail?.product.votesPerUser ?? 0) > 0}
                onChanged={reload}
              />
              <SecurityPanel issueId={id} onChanged={reload} />
              </fieldset>
              ) : null}
            </div>
          </div>
          <Stack className="issue-detail-side" gap={3}>
            <FieldsPanel
              issue={detail.issue}
              people={detail.people}
              product={product}
              component={component}
              productDetail={productDetail}
              products={
                productsQ.state.status === "ready"
                  ? productsQ.state.data.map((p) => ({ id: p.id, name: p.name }))
                  : []
              }
              fetchProductDetail={(pid) => toPromise(getProduct({ id: pid }))}
              onUpdated={() => reload()}
              readOnly={signedOut}
            />
            {/* State and relations, not narrative -- so their position must
                not depend on how long the conversation is. Below the composer
                they sank further down the page with every reply; an issue with
                forty comments put "who is CC'd" a thousand pixels from the
                top. Here they are anchored.

                Each one is a single line until asked to open (RailDisclosure),
                which is what makes this survivable: the last time these lived
                in the rail they rendered list AND form at all times and the
                stack measured 1856px in a 352px column. */}
            {/* Labelled like every other rail group, so the space above it
                reads as a heading's space rather than as a gap nobody meant.
                These four are about OTHER things -- people and issues -- which
                is a different kind of fact from severity or component. */}
            <p className="rail-section">Links</p>
            <fieldset className="rail-fields rail-groups" disabled={signedOut}>
              <KeywordsPanel issueId={id} activities={detail.activities} onChanged={reload} />
              <CcPanel issueId={id} />
              <DependenciesPanel issueId={id} />
              <DuplicatesPanel
                issueId={id}
                duplicateOfId={detail.issue.duplicateOfId ?? null}
                labels={historyLabels}
              />
              <SeeAlsoPanel issueId={id} />
            </fieldset>
          </Stack>
        </div>
      )}
    </div>
  );
}
