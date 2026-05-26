import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { getLivePreview } from "../api";
import { EmptyState } from "../components/EmptyState";

interface PreviewCanvasProps {
  appId?: string;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function PreviewCanvas({ appId }: PreviewCanvasProps) {
  const [reloadToken, setReloadToken] = useState(0);
  const [frameLoaded, setFrameLoaded] = useState(false);

  const previewQuery = useQuery({
    queryKey: ["live-preview", appId],
    queryFn: () => getLivePreview({ appId: appId! }),
    enabled: !!appId,
    retry: 1,
    staleTime: 30_000,
  });

  useEffect(() => {
    setFrameLoaded(false);
  }, [previewQuery.data?.url, reloadToken]);

  if (!appId) {
    return (
      <div data-testid="preview-canvas" className="h-full flex items-center justify-center bg-paper-2">
        <EmptyState
          title="Nothing's been built yet."
          description="Tell the agent what to make in the chat on the right."
        />
      </div>
    );
  }

  const previewUrl = previewQuery.data?.url;

  return (
    <div data-testid="preview-canvas" className="h-full min-h-0 flex flex-col bg-paper-2">
      <div className="shrink-0 h-10 border-b border-rule bg-paper flex items-center justify-between gap-3 px-3">
        <div className="min-w-0 flex items-center gap-2">
          <span
            className={[
              "size-[6px] rounded-full",
              previewQuery.isError ? "bg-tomato" : previewUrl ? "bg-ivy" : "bg-ink-soft",
            ].join(" ")}
            aria-hidden="true"
          />
          <span className="font-mono text-[11px] text-ink-soft truncate">
            {previewUrl ?? "starting sandbox preview"}
          </span>
        </div>
        <div className="shrink-0 flex items-center gap-2">
          <button
            type="button"
            onClick={() => {
              setFrameLoaded(false);
              setReloadToken((v) => v + 1);
              void previewQuery.refetch();
            }}
            className="px-2 py-1 border border-rule rounded-[4px] bg-paper-2 font-serif italic text-[12px] text-ink-soft hover:border-ink hover:text-ink focus:outline-2 focus:outline-tomato focus:outline-offset-2"
          >
            reload
          </button>
          {previewUrl && (
            <a
              href={previewUrl}
              target="_blank"
              rel="noreferrer"
              className="px-2 py-1 border border-rule rounded-[4px] bg-paper-2 font-serif italic text-[12px] text-ink-soft hover:border-ink hover:text-ink focus:outline-2 focus:outline-tomato focus:outline-offset-2"
              style={{ textDecoration: "none" }}
            >
              open
            </a>
          )}
        </div>
      </div>

      <div className="relative flex-1 min-h-0">
        {previewQuery.isLoading && (
          <div className="absolute inset-0 flex items-center justify-center px-6 text-center">
            <div className="font-serif italic text-ink-soft">Starting preview...</div>
          </div>
        )}

        {previewQuery.isError && (
          <div className="absolute inset-0 flex items-center justify-center px-6 text-center">
            <EmptyState
              title="Preview isn't running."
              description={errorMessage(previewQuery.error)}
            />
          </div>
        )}

        {previewUrl && (
          <>
            {!frameLoaded && (
              <div className="absolute inset-0 z-10 flex items-center justify-center px-6 text-center pointer-events-none">
                <div className="font-serif italic text-ink-soft">Loading preview...</div>
              </div>
            )}
            <iframe
              key={`${previewUrl}:${reloadToken}`}
              title="Live sandbox preview"
              data-testid="preview-frame"
              src={previewUrl}
              onLoad={() => setFrameLoaded(true)}
              className="absolute inset-0 size-full border-0 bg-white"
              sandbox="allow-forms allow-modals allow-popups allow-same-origin allow-scripts"
            />
          </>
        )}
      </div>
    </div>
  );
}
