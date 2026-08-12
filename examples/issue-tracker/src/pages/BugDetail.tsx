import { useEffect, useState } from "react";
import { getBug, getProduct, listProducts } from "../api";
import { ErrorState, Loading } from "../components/StateViews";
import { AttachmentsPanel } from "../components/bug-detail/AttachmentsPanel";
import { CcPanel } from "../components/bug-detail/CcPanel";
import { CommentsPanel } from "../components/bug-detail/CommentsPanel";
import { FieldsPanel } from "../components/bug-detail/FieldsPanel";
import { FlagsPanel } from "../components/bug-detail/FlagsPanel";
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

  return (
    <div className="page bug-detail-page">
      <div className="page-head">
        <h1>
          <span className="bug-id">{detail.bug.id}</span> {detail.bug.summary}
        </h1>
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
        <HistoryPanel activities={detail.activities} />
      ) : (
        <div className="bug-detail-grid">
          <div className="bug-detail-main">
            <FieldsPanel
              bug={detail.bug}
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
            <CommentsPanel bugId={id} />
          </div>
          <div className="bug-detail-side">
            <KeywordsPanel bugId={id} activities={detail.activities} onChanged={reload} />
            <FlagsPanel
              bugId={id}
              flagTypes={productDetail?.flagTypes ?? null}
              activities={detail.activities}
              onChanged={reload}
            />
            <DependenciesPanel bugId={id} />
            <DuplicatesPanel bugId={id} duplicateOfId={detail.bug.duplicateOfId ?? null} />
            <CcPanel bugId={id} />
            <AttachmentsPanel bugId={id} />
          </div>
        </div>
      )}
    </div>
  );
}
