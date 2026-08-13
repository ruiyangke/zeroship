import { useEffect, useState } from "react";
import { Card, Cluster, Stack } from "@zeroship/ui";
import { PriorityBadge, ResolutionBadge, SeverityBadge, StatusBadge } from "../components/Badges";
import { getBug, getProduct, listProducts } from "../api";
import { ErrorState, Loading } from "../components/StateViews";
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
      <div className="page-head">
        <h1>
          {/* The key, not the UUID. This heading is what a person copies into
              a commit message or reads out in a standup. */}
          <span className="bug-id">
            {detail.product ? `${detail.product.key}-${detail.bug.number}` : detail.bug.id}
          </span>{" "}
          {detail.bug.summary}
        </h1>
        {/* State belongs beside the title, not eleven fields down. Someone
            opening a bug asks "what is this and where is it up to" before
            anything else. */}
        <Cluster gap={2} align="center" className="bug-state">
          <StatusBadge status={detail.bug.status} />
          <ResolutionBadge resolution={detail.bug.resolution ?? null} />
          <SeverityBadge severity={detail.bug.severity} />
          <PriorityBadge priority={detail.bug.priority} />
        </Cluster>
      </div>

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
        <HistoryPanel activities={detail.activities} labels={historyLabels} />
      ) : (
        <div className="bug-detail-grid">
          {/* The conversation IS the bug. It used to sit under a screen of
              editable fields -- summary, status, severity, priority,
              assignee, product, component, version, whiteboard, OS, platform,
              URL -- so the description, which is what the bug actually says,
              started below the fold. Fields are metadata and metadata goes in
              the rail. */}
          <div className="bug-detail-main">
            <CommentsPanel bugId={id} />
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
            />
            <VotesPanel
              bugId={id}
              voteCount={detail.bug.voteCount}
              maxVotesPerBug={productDetail?.product.maxVotesPerBug ?? 0}
              votingEnabled={(productDetail?.product.votesPerUser ?? 0) > 0}
              onChanged={reload}
            />
            <KeywordsPanel bugId={id} activities={detail.activities} onChanged={reload} />
            {/* No `activities` prop: the panel reads real flags from
                flags.list instead of replaying the bug's history. */}
            <FlagsPanel
              bugId={id}
              flagTypes={productDetail?.flagTypes ?? null}
              onChanged={reload}
            />
            <SecurityPanel bugId={id} onChanged={reload} />
            <SeeAlsoPanel bugId={id} />
            <DependenciesPanel bugId={id} />
            <DuplicatesPanel bugId={id} duplicateOfId={detail.bug.duplicateOfId ?? null} />
            <CcPanel bugId={id} />
            <AttachmentsPanel bugId={id} />
          </Stack>
        </div>
      )}
    </div>
  );
}
