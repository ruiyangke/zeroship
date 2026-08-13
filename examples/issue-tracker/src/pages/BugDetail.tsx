import { useEffect, useState } from "react";
import { Banner, Card, Cluster, Stack } from "@zeroship/ui";
import { PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "../components/Badges";
import { currentUser, getBug, getProduct, listProducts } from "../api";
import { ErrorState, Loading } from "../components/StateViews";
import { isUnauthenticated } from "../components/rpc";
import { AttachmentsPanel } from "../components/bug-detail/AttachmentsPanel";
import { CcPanel } from "../components/bug-detail/CcPanel";
import { CommentsPanel } from "../components/bug-detail/CommentsPanel";
import { FieldsPanel } from "../components/bug-detail/FieldsPanel";
import { FlagsPanel } from "../components/bug-detail/FlagsPanel";
import { SecurityPanel } from "../components/bug-detail/SecurityPanel";
import { SeeAlsoPanel } from "../components/bug-detail/SeeAlsoPanel";
import { VotesPanel } from "../components/bug-detail/VotesPanel";
import { HistoryPanel } from "../components/bug-detail/HistoryPanel";
import { DependenciesPanel, DuplicatesPanel } from "../components/bug-detail/RelationsPanel";
import { KeywordsPanel } from "../components/bug-detail/KeywordsPanel";
import { toPromise, useAsync } from "../components/rpc";
import type { ProductDetail } from "../components/types";

type Tab = "details" | "history";

export function BugDetailPage({ id }: { id: string }) {
  const { state, reload } = useAsync(() => getBug({ id }), [id]);
  const [tab, setTab] = useState<Tab>("details");
  const [productDetail, setProductDetail] = useState<ProductDetail | null>(null);
  const productsQ = useAsync(() => listProducts({}), []);
  // Bugs are public, so unlike the dashboard this page stays READABLE without
  // an identity -- what it must not do is offer controls that cannot work.
  const { state: userState } = useAsync(() => currentUser({}), []);
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

  if (state.status === "loading") return <Loading label="Loading bug..." />;
  if (state.status === "error") return <ErrorState error={state.error} onRetry={reload} />;

  const { data: detail } = state;
  if (!detail.product || !detail.component) {
    return (
      <ErrorState
        error={new Error("This bug references a product or component that no longer resolves.")}
      />
    );
  }
  const product = detail.product;
  const component = detail.component;

  // Everything the log might name by id. Built from what the page already
  // has, so the history tab costs no extra request: the bug's own product and
  // component, the people bugs.get resolved, and the product structure the
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
  };

  return (
    <div className="page bug-detail-page">
      {/* The key, the title and the state, in that reading order.
          These used to share one line: a mono key, a long summary and four
          badges all at the same height, so a title that ran long squeezed the
          badges and the eye had no obvious place to start. The key is now a
          quiet line above, the summary owns its own line at heading size, and
          the state sits under it where it reads as a caption about the bug
          rather than as more title. */}
      <div className="page-head bug-head">
        <span className="bug-id">
          {detail.product ? `${detail.product.key}-${detail.bug.number}` : detail.bug.id}
        </span>
        <h1 className="bug-title">{detail.bug.summary}</h1>
        <Cluster gap={2} align="center" className="bug-state">
          <StatusBadge status={detail.bug.status} />
          <ResolutionBadge resolution={detail.bug.resolution ?? null} />
          <SeverityBadge severity={detail.bug.severity} />
          <PriorityBadge priority={detail.bug.priority} />
          <span className="bug-head-meta">
            {detail.product ? detail.product.name : null}
            {detail.component ? ` / ${detail.component.name}` : null}
          </span>
        </Cluster>
      </div>

      {/* Said once, at the top, rather than discovered one 401 at a time.
          A signed-out visitor could read this page and still be offered a
          comment box, an Edit button on every comment and three editable
          selects -- every one of which fails on use. The bug stays readable
          because bugs are public; the controls that cannot work go away. */}
      {signedOut ? (
        <Banner intent="info" title="You are not signed in">
          This bug is public, so you can read it. Sign in to comment or change any
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
        <div className="bug-detail-grid">
          {/* The conversation IS the bug. It used to sit under a screen of
              editable fields -- summary, status, severity, priority,
              assignee, product, component, version, whiteboard, OS, platform,
              URL -- so the description, which is what the bug actually says,
              started below the fold. Fields are metadata and metadata goes in
              the rail. */}
          <div className="bug-detail-main">
            <CommentsPanel bugId={id} readOnly={signedOut} />
            {/* Everything below is about the bug WITHOUT being metadata about
                it: files, links to other bugs, who is watching, what is
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
                linked bugs are readable facts about a public bug. */}
            <fieldset className="rail-fields bug-detail-extras" disabled={signedOut}>
              <AttachmentsPanel bugId={id} />
              <CcPanel bugId={id} />
              <DependenciesPanel bugId={id} />
              <DuplicatesPanel bugId={id} duplicateOfId={detail.bug.duplicateOfId ?? null} />
              <SeeAlsoPanel bugId={id} />
              <KeywordsPanel bugId={id} activities={detail.activities} onChanged={reload} />
              {/* No `activities` prop: the panel reads real flags from
                  flags.list instead of replaying the bug's history. */}
              <FlagsPanel bugId={id} flagTypes={productDetail?.flagTypes ?? null} onChanged={reload} />
              <VotesPanel
                bugId={id}
                voteCount={detail.bug.voteCount}
                maxVotesPerBug={productDetail?.product.maxVotesPerBug ?? 0}
                votingEnabled={(productDetail?.product.votesPerUser ?? 0) > 0}
                onChanged={reload}
              />
              <SecurityPanel bugId={id} onChanged={reload} />
            </fieldset>
          </div>
          <Stack className="bug-detail-side" gap={3}>
            <FieldsPanel
              bug={detail.bug}
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
          </Stack>
        </div>
      )}
    </div>
  );
}
