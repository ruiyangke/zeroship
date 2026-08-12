import { useState } from "react";
import { Select } from "@zeroship/ui";
import { createBug, currentUser, getProduct, listProducts } from "../api";
import { AsyncSection } from "../components/StateViews";
import { errorMessage, isUnauthenticated, useAsync } from "../components/rpc";
import type { ProductDetail } from "../components/types";
import { BUG_PRIORITIES, BUG_SEVERITIES, type BugPriority, type BugSeverity } from "../lib/quicksearch";

export function NewBugPage() {
  const { state: userState } = useAsync(() => currentUser({}), []);
  const productsQ = useAsync(() => listProducts({}), []);

  const [productId, setProductId] = useState("");
  const [productDetail, setProductDetail] = useState<ProductDetail | null>(null);
  const [productDetailError, setProductDetailError] = useState<string | null>(null);
  const [componentId, setComponentId] = useState("");
  const [versionId, setVersionId] = useState("");
  const [milestoneId, setMilestoneId] = useState("");
  const [summary, setSummary] = useState("");
  const [description, setDescription] = useState("");
  const [severity, setSeverity] = useState<BugSeverity>("normal");
  const [priority, setPriority] = useState<BugPriority>("P3");
  const [whiteboard, setWhiteboard] = useState("");
  const [opSys, setOpSys] = useState("Unspecified");
  const [platform, setPlatform] = useState("Unspecified");
  const [url, setUrl] = useState("");
  const [confirmed, setConfirmed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pickProduct = async (id: string) => {
    setProductId(id);
    setComponentId("");
    setVersionId("");
    setMilestoneId("");
    setProductDetail(null);
    setProductDetailError(null);
    if (!id) return;
    try {
      setProductDetail(await getProduct({ id }));
    } catch (err) {
      setProductDetailError(errorMessage(err));
    }
  };

  const submit = async () => {
    if (!productId || !componentId || !summary.trim() || !description.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const bug = await createBug({
        productId,
        componentId,
        summary: summary.trim(),
        description: description.trim(),
        versionId: versionId || undefined,
        milestoneId: milestoneId || undefined,
        severity,
        priority,
        whiteboard: whiteboard || undefined,
        opSys,
        platform,
        url: url || undefined,
        confirmed: confirmed || undefined,
      });
      window.location.hash = `#/bugs/${bug.id}`;
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  if (userState.status === "error" && isUnauthenticated(userState.error)) {
    return (
      <div className="page">
        <h1>New bug</h1>
        <p className="state-hint">Filing a bug requires a signed-in identity. Sign in and reload.</p>
      </div>
    );
  }

  return (
    <div className="page new-bug-page">
      <h1>New bug</h1>
      <AsyncSection state={productsQ.state} loadingLabel="Loading products..." isEmpty={(d) => d.length === 0} emptyTitle="No products to file against.">
        {(products) => (
          <form
            className="new-bug-form"
            onSubmit={(e) => {
              e.preventDefault();
              void submit();
            }}
          >
            <label>
              1. Product
              <Select
                value={productId}
                onValueChange={(next) => void pickProduct(next ?? "")}
                placeholder="Select a product"
                aria-label="1. Product"
              >
                {products.map((p) => (
                  <Select.Item key={p.id} value={p.id}>
                    {p.name}
                  </Select.Item>
                ))}
              </Select>
            </label>
            {productDetailError ? <p className="field-error">{productDetailError}</p> : null}

            {productDetail ? (
              <>
                <label>
                  2. Component
                  <Select
                value={componentId}
                onValueChange={(next) => setComponentId(next ?? "")}
                placeholder="Select a component"
                aria-label="2. Component"
              >
                {productDetail.components.map((c) => (
                  <Select.Item key={c.id} value={c.id}>
                    {c.name}
                  </Select.Item>
                ))}
              </Select>
                </label>

                <fieldset disabled={!componentId}>
                  <legend>3. Details</legend>
                  <label>
                    Summary
                    <input value={summary} onChange={(e) => setSummary(e.target.value)} maxLength={500} required />
                  </label>
                  <label>
                    Description
                    <textarea
                      value={description}
                      onChange={(e) => setDescription(e.target.value)}
                      rows={6}
                      required
                    />
                  </label>
                  <div className="field-row">
                    <label>
                      Version
                      {/* Required, not optional: bugs.versionId is NOT NULL,
                          so an "unspecified" choice here composed a bug that
                          could not be stored and failed at submit. Milestone
                          below is genuinely nullable and keeps its blank. */}
                      <Select
                value={versionId}
                onValueChange={(next) => setVersionId(next ?? "")}
                placeholder="Select a version"
                aria-label="Version"
              >
                        {productDetail.versions.map((v) => (
                          <Select.Item key={v.id} value={v.id}>
                            {v.name}
                          </Select.Item>
                        ))}
                      </Select>
                    </label>
                    <label>
                      Milestone
                      <Select
                        value={milestoneId}
                        onValueChange={(next) => setMilestoneId(next ?? "")}
                        placeholder="unspecified"
                aria-label="Milestone"
                      >
                        {productDetail.milestones.map((m) => (
                          <Select.Item key={m.id} value={m.id}>
                            {m.name}
                          </Select.Item>
                        ))}
                      </Select>
                    </label>
                  </div>
                  <div className="field-row">
                    <label>
                      Severity
                      <Select
                        value={severity}
                        onValueChange={(next) => setSeverity(next as BugSeverity)}
                        aria-label="Severity"
                      >
                        {BUG_SEVERITIES.map((s) => (
                          <Select.Item key={s} value={s}>
                            {s}
                          </Select.Item>
                        ))}
                      </Select>
                    </label>
                    <label>
                      Priority
                      <Select
                        value={priority}
                        onValueChange={(next) => setPriority(next as BugPriority)}
                        aria-label="Priority"
                      >
                        {BUG_PRIORITIES.map((p) => (
                          <Select.Item key={p} value={p}>
                            {p}
                          </Select.Item>
                        ))}
                      </Select>
                    </label>
                  </div>
                  <div className="field-row">
                    <label>
                      OS
                      <input value={opSys} onChange={(e) => setOpSys(e.target.value)} />
                    </label>
                    <label>
                      Platform
                      <input value={platform} onChange={(e) => setPlatform(e.target.value)} />
                    </label>
                  </div>
                  <label>
                    Whiteboard
                    <input value={whiteboard} onChange={(e) => setWhiteboard(e.target.value)} />
                  </label>
                  <label>
                    URL
                    <input value={url} onChange={(e) => setUrl(e.target.value)} />
                  </label>
                  {productDetail.product.allowsUnconfirmed ? (
                    <label className="checkbox-label">
                      <input type="checkbox" checked={confirmed} onChange={(e) => setConfirmed(e.target.checked)} />
                      File as CONFIRMED (skip UNCONFIRMED)
                    </label>
                  ) : (
                    <p className="state-hint small">
                      This product does not allow UNCONFIRMED bugs; this will be filed as CONFIRMED.
                    </p>
                  )}

                  <button
                    type="submit"
                    className="btn primary"
                    disabled={busy || !summary.trim() || !description.trim()}
                  >
                    {busy ? "Filing..." : "File bug"}
                  </button>
                  {error ? <p className="field-error">{error}</p> : null}
                </fieldset>
              </>
            ) : null}
          </form>
        )}
      </AsyncSection>
    </div>
  );
}
