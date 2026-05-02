// ─── MediaCanvas — drop zone + grid (spec §9.4) ─────────────────
//
// One canvas, two regions:
//   1. Drop zone — drag-drop or click-to-pick. Reads the file as
//      base64 and POSTs via uploadMedia. Multiple files queue up
//      sequentially (cheap; the in-memory stub doesn't care).
//   2. Grid — thumbnails fetched via listMedia. Image MIME types
//      render the URL inline; everything else gets a file-icon tile.
//      Hover reveals copy-URL + delete buttons.
//
// All data flows through the in-memory stub in `src/server/agents.ts`
// (ISS-26). When the real `@zeroship/storage` upload RPC lands, swap
// the procs out — the wire shape (single-input objects) stays.

import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  listMedia,
  uploadMedia,
  deleteMedia,
  type MediaEntry,
} from "../../api";

export interface MediaCanvasProps {
  appId: string;
}

export function MediaCanvas({ appId }: MediaCanvasProps) {
  const qc = useQueryClient();
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [uploadError, setUploadError] = useState<string | null>(null);

  const { data, isLoading } = useQuery({
    queryKey: ["media", appId],
    queryFn: () => listMedia({ appId }),
    retry: false,
  });

  const upload = useMutation({
    mutationFn: async (files: File[]) => {
      // Sequential — keeps the order predictable in the in-memory
      // store and avoids a thundering-herd on the dev server.
      for (const file of files) {
        const base64 = await readAsBase64(file);
        await uploadMedia({
          appId,
          name: file.name,
          contentType: file.type || "application/octet-stream",
          base64,
        });
      }
    },
    onSuccess: () => {
      setUploadError(null);
      qc.invalidateQueries({ queryKey: ["media", appId] });
    },
    onError: (e) => {
      setUploadError((e as Error).message ?? "upload failed");
    },
  });

  const remove = useMutation({
    mutationFn: (key: string) => deleteMedia({ appId, key }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["media", appId] });
    },
  });

  function handleFiles(filesList: FileList | null) {
    if (!filesList || filesList.length === 0) return;
    const files = Array.from(filesList);
    upload.mutate(files);
  }

  const items = data?.items ?? [];

  return (
    <div data-testid="media-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[960px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <header className="mb-6">
          <h1 className="font-serif italic font-medium text-[28px] m-0 mb-1">
            Media
          </h1>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            Images, videos, and other files your project serves.
            Uploads are in-memory in V1 — see ISSUES.md ISS-26.
          </p>
        </header>

        <button
          type="button"
          data-testid="media-dropzone"
          onClick={() => fileInputRef.current?.click()}
          onDragOver={(e) => {
            e.preventDefault();
            setDragOver(true);
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragOver(false);
            handleFiles(e.dataTransfer.files);
          }}
          className={
            "w-full block border-2 border-dashed py-10 px-6 text-center bg-transparent cursor-pointer transition-colors mb-6 " +
            (dragOver
              ? "border-tomato bg-tomato/5"
              : "border-rule hover:border-ink")
          }
        >
          <div className="font-serif italic text-[16px] text-ink mb-1">
            {upload.isPending
              ? "Uploading…"
              : "Drop files here or click to upload."}
          </div>
          <div className="font-serif italic text-[12.5px] text-pencil">
            Anything goes — images, video, audio, PDFs.
          </div>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            data-testid="media-file-input"
            onChange={(e) => {
              handleFiles(e.target.files);
              // Reset so picking the same file twice still triggers onChange.
              if (e.target) e.target.value = "";
            }}
            className="hidden"
          />
        </button>

        {uploadError && (
          <div
            data-testid="media-upload-error"
            className="mb-4 px-3 py-2 border border-tomato/40 bg-tomato/5 font-serif italic text-[13px] text-tomato"
          >
            {uploadError}
          </div>
        )}

        {isLoading && (
          <div className="font-serif italic text-pencil py-2">loading…</div>
        )}

        {!isLoading && items.length === 0 && (
          <div
            data-testid="media-empty"
            className="font-serif italic text-pencil py-10 text-center border border-rule-2 bg-paper-2/40"
          >
            No files yet — drop one above to get started.
          </div>
        )}

        {items.length > 0 && (
          <div
            data-testid="media-grid"
            className="grid gap-4"
            style={{ gridTemplateColumns: "repeat(auto-fill, minmax(180px, 1fr))" }}
          >
            {items.map((m) => (
              <MediaTile
                key={m.key}
                item={m}
                onDelete={() => remove.mutate(m.key)}
              />
            ))}
          </div>
        )}

        <div className="mt-6 font-serif italic text-[12px] text-pencil">
          Real `@zeroship/storage` backing tracked as ISS-26.
        </div>
      </div>
    </div>
  );
}

// ─── tile ───────────────────────────────────────────────────────

function MediaTile({
  item,
  onDelete,
}: {
  item: MediaEntry;
  onDelete: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const isImage = item.contentType.startsWith("image/");

  async function copyUrl() {
    try {
      await navigator.clipboard.writeText(item.url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      // Clipboard may be denied (no HTTPS, headless test). Best-effort.
    }
  }

  return (
    <div
      data-testid={`media-tile:${item.key}`}
      className="group border border-rule-2 bg-paper-2/40 overflow-hidden flex flex-col"
    >
      <div className="relative aspect-square bg-paper-2 overflow-hidden">
        {isImage ? (
          // Images: show the actual bytes. data: URLs work as-is.
          // External URLs (sample seeds) work in dev; if they 404 the
          // alt text falls back via the broken-image icon.
          // eslint-disable-next-line @next/next/no-img-element
          <img
            src={item.url}
            alt={item.name}
            className="w-full h-full object-cover"
            loading="lazy"
          />
        ) : (
          <FileIcon contentType={item.contentType} />
        )}
        {/* Hover overlay with copy + delete actions. */}
        <div className="absolute inset-0 bg-ink/0 group-hover:bg-ink/40 transition-colors flex items-center justify-center gap-2 opacity-0 group-hover:opacity-100">
          <button
            type="button"
            onClick={copyUrl}
            data-testid={`media-tile-copy:${item.key}`}
            className="font-sans text-[10px] uppercase tracking-[0.16em] text-paper bg-ink/70 hover:bg-ink px-2 py-1 border-0 cursor-pointer"
          >
            {copied ? "copied" : "copy url"}
          </button>
          <button
            type="button"
            onClick={onDelete}
            data-testid={`media-tile-delete:${item.key}`}
            className="font-sans text-[10px] uppercase tracking-[0.16em] text-paper bg-tomato hover:opacity-90 px-2 py-1 border-0 cursor-pointer"
          >
            delete
          </button>
        </div>
      </div>
      <div className="px-3 py-2">
        <div className="font-serif text-[13px] text-ink truncate" title={item.name}>
          {item.name}
        </div>
        <div className="font-mono text-[11px] text-pencil mt-0.5">
          {fmtBytes(item.size)}
        </div>
      </div>
    </div>
  );
}

function FileIcon({ contentType }: { contentType: string }) {
  // Pick a one-letter tag from the MIME family for a readable, no-svg
  // placeholder. Keeps the canvas legible at thumbnail scale.
  const tag = contentType.startsWith("video/")
    ? "VIDEO"
    : contentType.startsWith("audio/")
      ? "AUDIO"
      : contentType.includes("pdf")
        ? "PDF"
        : "FILE";
  return (
    <div className="w-full h-full flex items-center justify-center bg-paper-2">
      <span className="font-sans text-[11px] uppercase tracking-[0.18em] text-ink-soft">
        {tag}
      </span>
    </div>
  );
}

// ─── helpers ────────────────────────────────────────────────────

async function readAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      // FileReader returns "data:<mime>;base64,<...>"; strip the prefix
      // because uploadMedia expects raw base64 only.
      const result = String(reader.result ?? "");
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.readAsDataURL(file);
  });
}

function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}
