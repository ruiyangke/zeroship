import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button, Card, EmptyState, Spinner } from "@zeroship/ui";
import { getLivePreview } from "../api";
import "./PreviewCanvas.css";

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
      <div data-testid="preview-canvas" className="zs-preview-canvas zs-preview-canvas--center">
        <EmptyState
          title="Nothing's been built yet."
          description="Tell the agent what to make in the chat on the right."
        />
      </div>
    );
  }

  const previewUrl = previewQuery.data?.url;
  const status: "error" | "live" | "pending" = previewQuery.isError
    ? "error"
    : previewUrl
      ? "live"
      : "pending";

  return (
    <div data-testid="preview-canvas" className="zs-preview-canvas">
      <Card variant="outline" className="zs-preview-frame">
        <div className="zs-preview-bar">
          <div className="zs-preview-bar__id">
            <span
              className="zs-preview-bar__dot"
              data-status={status}
              aria-hidden="true"
            />
            <span className="zs-preview-bar__url">
              {previewUrl ?? "starting sandbox preview"}
            </span>
          </div>
          <div className="zs-preview-bar__actions">
            <Button
              type="button"
              variant="plain"
              size="small"
              onClick={() => {
                setFrameLoaded(false);
                setReloadToken((v) => v + 1);
                void previewQuery.refetch();
              }}
            >
              reload
            </Button>
            {previewUrl && (
              <Button asChild variant="plain" size="small">
                <a href={previewUrl} target="_blank" rel="noreferrer">
                  open
                </a>
              </Button>
            )}
          </div>
        </div>

        <div className="zs-preview-stage">
          {previewQuery.isLoading && (
            <div className="zs-preview-overlay">
              <Spinner label="Starting preview" />
            </div>
          )}

          {previewQuery.isError && (
            <div className="zs-preview-overlay">
              <EmptyState
                title="Preview isn't running."
                description={errorMessage(previewQuery.error)}
              />
            </div>
          )}

          {previewUrl && (
            <>
              {!frameLoaded && (
                <div className="zs-preview-overlay zs-preview-overlay--ghost">
                  <Spinner label="Loading preview" />
                </div>
              )}
              <iframe
                key={`${previewUrl}:${reloadToken}`}
                title="Live sandbox preview"
                data-testid="preview-frame"
                src={previewUrl}
                onLoad={() => setFrameLoaded(true)}
                className="zs-preview-iframe"
                sandbox="allow-forms allow-modals allow-popups allow-same-origin allow-scripts"
              />
            </>
          )}
        </div>
      </Card>
    </div>
  );
}
