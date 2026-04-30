// ─── PreviewTab — the artifact, framed as a printed plate ───────
//
// Default tab. Iframe pointed at the app's preview URL, with a
// minimal chrome bar above (URL + reload + open-in-tab). Reload key
// from workspace context bumps on every successful deploy.

import { useEffect, useRef, useState } from "react";
import { appPreviewUrl } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { TabDrawer } from "../components/TabDrawer";

export function PreviewTab() {
  const { app, appId, deployVersion } = useWorkspace();
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const [loading, setLoading] = useState(true);
  const [errored, setErrored] = useState(false);

  const src = app?.name && app.deploy_hash
    ? `${appPreviewUrl(app.name)}?_v=${deployVersion}`
    : null;

  useEffect(() => {
    setLoading(true); setErrored(false);
  }, [src]);

  function reload() {
    setLoading(true); setErrored(false);
    const f = iframeRef.current;
    if (!f) return;
    try { f.contentWindow?.location.reload(); }
    catch {
      const cur = f.src; f.src = "about:blank";
      requestAnimationFrame(() => { f.src = cur; });
    }
  }

  return (
    <div className="h-full flex flex-col" data-testid="preview-tab">
      {/* the printed-plate frame */}
      <div className="flex-1 px-8 pt-6 pb-0 flex flex-col min-h-0">
        <div className="flex-1 bg-white border border-rule shadow-[0_28px_36px_-22px_rgba(34,22,12,0.16),0_8px_16px_-10px_rgba(34,22,12,0.08)] flex flex-col overflow-hidden">
          {/* plate bar */}
          <div className="flex items-center gap-3 px-3.5 py-2 bg-paper-2 border-b border-rule font-sans text-[10.5px] text-ink-soft">
            <div className="inline-flex gap-1">
              <span className="size-[8px] rounded-full bg-tomato/70" />
              <span className="size-[8px] rounded-full bg-rule" />
              <span className="size-[8px] rounded-full bg-rule" />
            </div>
            <div className="flex-1 text-center font-mono text-[11px]">
              {app?.name ? <>https://<strong className="text-ink font-medium">{app.name}.zeroship.app</strong></> : "—"}
            </div>
            <button
              type="button"
              onClick={reload}
              disabled={!src}
              title="Reload"
              className="font-sans text-[10px] uppercase tracking-[0.14em] text-pencil hover:text-ink bg-transparent border-0 cursor-pointer disabled:opacity-50"
            >
              {loading ? "…" : "↻"} reload
            </button>
            {src && (
              <a
                href={src}
                target="_blank"
                rel="noreferrer"
                title="Open in new tab"
                className="font-sans text-[10px] uppercase tracking-[0.14em] text-pencil hover:text-ink"
                style={{ textDecoration: "none" }}
              >
                ↗ open
              </a>
            )}
          </div>
          {/* iframe or placeholder */}
          <div className="flex-1 relative bg-white">
            {src ? (
              <>
                <iframe
                  ref={iframeRef}
                  src={src}
                  className="absolute inset-0 w-full h-full border-0 bg-white"
                  sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals"
                  onLoad={() => setLoading(false)}
                  onError={() => { setLoading(false); setErrored(true); }}
                  title={`Preview of ${app?.name ?? "app"}`}
                />
                {loading && (
                  <div className="absolute inset-x-0 top-0 h-0.5 bg-tomato animate-pulse" aria-hidden="true" />
                )}
                {errored && (
                  <div className="absolute inset-0 flex items-center justify-center bg-paper/95">
                    <div className="font-serif italic text-tomato">preview failed to load</div>
                  </div>
                )}
              </>
            ) : (
              <div className="absolute inset-0 flex items-center justify-center">
                <div className="text-center max-w-md px-6">
                  <div className="font-serif italic text-[18px] text-ink-soft mb-2">
                    Nothing's been built yet.
                  </div>
                  <div className="font-serif text-[14px] text-pencil">
                    Tell the studio what to make in the chat on the right.
                  </div>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
      <TabDrawer
        appId={appId}
        meta={
          app?.deploy_hash
            ? <>last shipped just now</>
            : <em>not yet deployed</em>
        }
      />
    </div>
  );
}
