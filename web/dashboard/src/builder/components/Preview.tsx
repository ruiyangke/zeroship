// ─── Preview ─────────────────────────────────────────────────────
// Iframe pointed at the deployed app's preview URL. Exposes a
// reload() method via the imperative-handle ref so the parent can
// force a refresh after the agent deploys. We also surface a small
// status strip with the URL + manual reload + open-in-new-tab.

import { forwardRef, useImperativeHandle, useRef, useState, useEffect } from "react";
import { ExternalLink, RotateCw, AlertCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { appPreviewUrl } from "../../api";

export interface PreviewHandle {
  reload: () => void;
}

interface Props {
  appName: string | null;
  /** Bumped by the parent on each successful deploy → forces a full re-mount. */
  deployVersion: number;
}

export const Preview = forwardRef<PreviewHandle, Props>(function Preview(
  { appName, deployVersion },
  ref,
) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const [loading, setLoading] = useState(true);
  const [errored, setErrored] = useState(false);

  // src includes deployVersion so a successful deploy triggers a re-mount,
  // sidestepping any iframe-cache weirdness across reloads.
  const src = appName ? `${appPreviewUrl(appName)}?_v=${deployVersion}` : "about:blank";

  useImperativeHandle(ref, () => ({
    reload: () => {
      setLoading(true);
      setErrored(false);
      const f = iframeRef.current;
      if (!f) return;
      try {
        // Same-origin (path-based routing on dashboard) → can call reload directly.
        f.contentWindow?.location.reload();
      } catch {
        // Cross-origin — fall back to src reassignment.
        const cur = f.src;
        f.src = "about:blank";
        requestAnimationFrame(() => {
          f.src = cur;
        });
      }
    },
  }), []);

  useEffect(() => {
    setLoading(true);
    setErrored(false);
  }, [src]);

  if (!appName) {
    return (
      <div className="h-full flex flex-col bg-background">
        <PreviewBar appName={null} src={null} onReload={() => {}} />
        <div className="flex-1 flex items-center justify-center text-xs text-muted-foreground">
          no preview — create or deploy an app first
        </div>
      </div>
    );
  }

  return (
    <div className="h-full flex flex-col bg-background">
      <PreviewBar
        appName={appName}
        src={src}
        loading={loading}
        onReload={() => {
          setLoading(true);
          iframeRef.current?.contentWindow?.location.reload();
        }}
      />
      <div className="flex-1 relative bg-white dark:bg-zinc-900">
        <iframe
          ref={iframeRef}
          src={src}
          className="absolute inset-0 w-full h-full border-0 bg-white"
          sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals"
          onLoad={() => setLoading(false)}
          onError={() => { setLoading(false); setErrored(true); }}
          title={`Preview of ${appName}`}
        />
        {loading && (
          <div className="absolute inset-x-0 top-0 h-0.5 bg-primary animate-pulse" />
        )}
        {errored && (
          <div className="absolute inset-0 flex items-center justify-center bg-background/95">
            <div className="flex items-center gap-2 text-xs text-destructive">
              <AlertCircle className="size-4" />
              preview failed to load
            </div>
          </div>
        )}
      </div>
    </div>
  );
});

interface BarProps {
  appName: string | null;
  src: string | null;
  loading?: boolean;
  onReload: () => void;
}

function PreviewBar({ appName, src, loading, onReload }: BarProps) {
  return (
    <div className="border-b border-border bg-muted/30 px-3 py-1.5 flex items-center gap-2 text-xs font-mono text-muted-foreground">
      <span className="size-2 rounded-full bg-emerald-500" />
      <span className="truncate flex-1">
        {appName ? `${appName}` : "—"}
        {src && <span className="text-muted-foreground/60 ml-2">{src}</span>}
      </span>
      <Button
        type="button"
        variant="ghost"
        onClick={onReload}
        disabled={!appName}
        title="Reload"
        className="h-6 w-6 p-0"
      >
        <RotateCw className={`size-3 ${loading ? "animate-spin" : ""}`} />
      </Button>
      {src && (
        <a
          href={src}
          target="_blank"
          rel="noreferrer"
          title="Open in new tab"
          className="inline-flex items-center justify-center h-6 w-6 hover:bg-muted transition-colors"
        >
          <ExternalLink className="size-3" />
        </a>
      )}
    </div>
  );
}
