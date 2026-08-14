import { useState } from "react";
import { formatBytes } from "./attachments";
import { Button } from "@zeroship/ui";
import { deleteAttachment, listAttachments, setAttachmentObsolete } from "../../api";
import { downloadAttachment } from "../../lib/download";
import { AsyncSection } from "../StateViews";
import { errorMessage, type AsyncState } from "../rpc";
import type { Attachment } from "../types";

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
      await downloadAttachment(attachment.id, attachment.filename);
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
        {/* Plain, not destructive-red. On a list row this was the most
            saturated thing in the whole column -- louder than "Comment", the
            page primary -- for an action on a file somebody attached on
            purpose. Deleting is available here, it is not the point of the
            panel. */}
        <Button variant="plain" size="small" disabled={busy} onClick={() => void remove()}>
          Delete
        </Button>
      </div>
      {attachment.description ? <p className="attachment-description">{attachment.description}</p> : null}
      {error ? <p className="field-error">{error}</p> : null}
    </li>
  );
}

/**
 * Every file on the bug, in one place.
 *
 * Uploading happens in the comment box now -- a file almost always needs a
 * sentence saying what it is, and this panel had you do the two things at
 * opposite ends of the page. What it keeps is the ROLL-UP: comments scatter
 * files down a long thread, and "what has been attached to this bug" is still
 * a question worth answering in one glance. Files that predate a comment, or
 * arrived without one, appear here and nowhere else.
 */
export function AttachmentsPanel({
  state,
  reload,
}: {
  // Handed in rather than fetched. This panel and the comment thread both
  // list the same files, and each owning its own query meant posting a
  // comment with an attachment refreshed the thread and left this panel
  // saying "No files yet" beside the file it was denying. One fetch, one
  // owner, both views current -- the same fix the unread badge needed when
  // two components derived a count from different sources.
  state: AsyncState<Attachment[]>;
  reload: () => void;
}) {

  return (
    <section className="attachments-panel">
      <h3>Files</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading attachments..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No files yet. Attach one by adding a comment."
        emptyTone="inline"
      >
        {(attachments: Attachment[]) => (
          <ul className="attachment-list">
            {attachments.map((attachment) => (
              <AttachmentRow key={attachment.id} attachment={attachment} onChanged={reload} />
            ))}
          </ul>
        )}
      </AsyncSection>
      {state.status === "ready" && state.data.length > 0 ? (
        <p className="state-hint small">Attach files by adding a comment.</p>
      ) : null}
    </section>
  );
}
