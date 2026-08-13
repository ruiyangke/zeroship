import { useRef, useState } from "react";
import { Button, Checkbox, Input } from "@zeroship/ui";
import { deleteAttachment, getAttachment, listAttachments, setAttachmentObsolete, uploadAttachment } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import type { Attachment } from "../types";

// Mirrors the server's MAX_ATTACHMENT_BYTES so oversized files fail fast in
// the browser instead of round-tripping to a 413. The server remains the
// enforced limit; this is a UX shortcut, not a security boundary.
const MAX_ATTACHMENT_BYTES = 512 * 1024;

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result as string;
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.onerror = () => reject(reader.error ?? new Error("failed to read file"));
    reader.readAsDataURL(file);
  });
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function AttachmentRow({ attachment, onChanged }: { attachment: Attachment; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);

  const toggleObsolete = async () => {
    setBusy(true);
    setError(null);
    try {
      await setAttachmentObsolete({ id: attachment.id, isObsolete: !attachment.isObsolete });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await deleteAttachment({ id: attachment.id });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const download = async () => {
    setDownloading(true);
    setError(null);
    try {
      const { contentBase64, contentType } = await getAttachment({ id: attachment.id });
      const bytes = Uint8Array.from(atob(contentBase64), (c) => c.charCodeAt(0));
      const blob = new Blob([bytes], { type: contentType });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = attachment.filename;
      link.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setDownloading(false);
    }
  };

  return (
    <li className={`attachment ${attachment.isObsolete ? "obsolete" : ""}`}>
      <div className="attachment-head">
        <Button variant="gray" size="small" disabled={downloading} onClick={() => void download()}>
          {downloading ? "..." : attachment.filename}
        </Button>
        <span className="dim">{formatBytes(attachment.sizeBytes)}</span>
        <span className="dim">{attachment.contentType}</span>
        {attachment.isPatch ? <span className="badge patch-badge">patch</span> : null}
        {attachment.isObsolete ? <span className="badge obsolete-badge">obsolete</span> : null}
        <Button variant="gray" size="small" disabled={busy} onClick={() => void toggleObsolete()}>
          {attachment.isObsolete ? "Un-obsolete" : "Mark obsolete"}
        </Button>
        <Button variant="gray" size="small" intent="destructive" disabled={busy} onClick={() => void remove()}>
          Delete
        </Button>
      </div>
      {attachment.description ? <p className="attachment-description">{attachment.description}</p> : null}
      {error ? <p className="field-error">{error}</p> : null}
    </li>
  );
}

function UploadForm({ bugId, onUploaded }: { bugId: string; onUploaded: () => void }) {
  const fileRef = useRef<HTMLInputElement>(null);
  const [description, setDescription] = useState("");
  const [isPatch, setIsPatch] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    const file = fileRef.current?.files?.[0];
    if (!file) return;
    if (file.size > MAX_ATTACHMENT_BYTES) {
      setError(`File is ${formatBytes(file.size)}; the server caps attachments at ${formatBytes(MAX_ATTACHMENT_BYTES)}.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const contentBase64 = await fileToBase64(file);
      await uploadAttachment({
        bugId,
        filename: file.name,
        contentBase64,
        contentType: file.type || "application/octet-stream",
        description: description || undefined,
        isPatch,
      });
      setDescription("");
      setIsPatch(false);
      if (fileRef.current) fileRef.current.value = "";
      onUploaded();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="upload-form">
      <input ref={fileRef} type="file" disabled={busy} aria-label="Attachment file" />
      <Input
        aria-label="Attachment description"
        placeholder="Description (optional)"
        value={description}
        onChange={(e) => setDescription(e.target.value)}
      />
      {/* No wrapping <label>: Checkbox renders its own, and nesting one
          inside another associates the control twice. */}
      <Checkbox
        checked={isPatch}
        onCheckedChange={(next: boolean | "indeterminate") => setIsPatch(next === true)}
        label="Patch"
      />
      <Button variant="filled" size="small" disabled={busy} onClick={() => void submit()}>
        Upload
      </Button>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

export function AttachmentsPanel({ bugId }: { bugId: string }) {
  const { state, reload } = useAsync(() => listAttachments({ bugId }), [bugId]);

  return (
    <section className="attachments-panel">
      <h3>Attachments</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading attachments..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No attachments yet."
      >
        {(attachments) => (
          <ul className="attachment-list">
            {attachments.map((attachment) => (
              <AttachmentRow key={attachment.id} attachment={attachment} onChanged={reload} />
            ))}
          </ul>
        )}
      </AsyncSection>
      {state.status !== "error" ? <UploadForm bugId={bugId} onUploaded={reload} /> : null}
    </section>
  );
}
