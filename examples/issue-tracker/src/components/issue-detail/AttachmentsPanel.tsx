import { useState } from "react";
import { formatBytes } from "./attachments";
import { Button } from "../../ui/Button";
import { deleteAttachment, setAttachmentObsolete } from "../../api";
import { downloadAttachment } from "../../lib/download";
import { AsyncSection } from "../StateViews";
import { errorMessage } from "../rpc";
import { useAppMutation, useAttachments } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
import type { Attachment } from "../types";
import { FieldError, Hint, Muted } from "../AppPrimitives";
import { Badge } from "../../ui/Badge";
import { DetailPanel } from "./DetailPanel";

function AttachmentRow({ attachment }: { attachment: Attachment }) {
  const [error, setError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);

  // A file changing state is an attachment change: the roll-up here, the same
  // files rendered inline in the thread, and the issue itself all hear about
  // it from one declaration.
  const setObsolete = useAppMutation(
    (isObsolete: boolean) => setAttachmentObsolete({ id: attachment.id, isObsolete }),
    () => invalidatedBy.attachmentChanged(attachment.issueId),
  );
  const removeFile = useAppMutation(
    () => deleteAttachment({ id: attachment.id }),
    () => invalidatedBy.attachmentChanged(attachment.issueId),
  );
  const busy = setObsolete.isPending || removeFile.isPending;

  const toggleObsolete = async () => {
    setError(null);
    try {
      await setObsolete.mutateAsync(!attachment.isObsolete);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const remove = async () => {
    setError(null);
    try {
      await removeFile.mutateAsync(undefined);
    } catch (err) {
      setError(errorMessage(err));
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
    <li
      className={`border-t border-line py-1 first:border-t-0 ${attachment.isObsolete ? "opacity-55" : ""}`}
    >
      <div className="flex flex-wrap items-center gap-2 text-base">
        <Button variant="gray" disabled={downloading} onClick={() => void download()}>
          {downloading ? "..." : attachment.filename}
        </Button>
        <Muted>{formatBytes(attachment.sizeBytes)}</Muted>
        <Muted>{attachment.contentType}</Muted>
        {attachment.isPatch ? <Badge intent="info">patch</Badge> : null}
        {attachment.isObsolete ? <Badge intent="muted">obsolete</Badge> : null}
        <Button variant="gray" disabled={busy} onClick={() => void toggleObsolete()}>
          {attachment.isObsolete ? "Un-obsolete" : "Mark obsolete"}
        </Button>
        {/* Plain, not destructive-red. On a list row this was the most
            saturated thing in the whole column -- louder than "Comment", the
            page primary -- for an action on a file somebody attached on
            purpose. Deleting is available here, it is not the point of the
            panel. */}
        <Button variant="plain" disabled={busy} onClick={() => void remove()}>
          Delete
        </Button>
      </div>
      {attachment.description ? (
        <p className="mt-1 text-base text-ink-secondary">{attachment.description}</p>
      ) : null}
      {error ? <FieldError>{error}</FieldError> : null}
    </li>
  );
}

/**
 * Every file on the issue, in one place.
 *
 * Uploading happens in the comment box now -- a file almost always needs a
 * sentence saying what it is, and this panel had you do the two things at
 * opposite ends of the page. What it keeps is the ROLL-UP: comments scatter
 * files down a long thread, and "what has been attached to this issue" is still
 * a question worth answering in one glance. Files that predate a comment, or
 * arrived without one, appear here and nowhere else.
 */
export function AttachmentsPanel({ issueId }: { issueId: string }) {
  // Fetched here, and fetched again by the comment thread, which lists the
  // same files per comment. That used to be one query owned by the page and
  // drilled into both, because two independent `useAsync` calls meant posting
  // a comment with an attachment refreshed the thread and left this panel
  // saying "No files yet" beside the file it was denying. Both callers now ask
  // the cache the same question under the same key, so there is still ONE
  // request and one answer -- but no prop to keep in sync, and an upload
  // anywhere invalidates the key rather than having to find the owner.
  const attachmentsQ = useAttachments(issueId);

  return (
    <DetailPanel locator="attachments-panel" title="Files">
      <AsyncSection
        query={attachmentsQ}
        loadingLabel="Loading attachments..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No files yet. Attach one by adding a comment."
        emptyTone="inline"
      >
        {(attachments: Attachment[]) => (
          <ul className="mb-3 flex list-none flex-col gap-2 p-0">
            {attachments.map((attachment) => (
              <AttachmentRow key={attachment.id} attachment={attachment} />
            ))}
          </ul>
        )}
      </AsyncSection>
      {attachmentsQ.data && attachmentsQ.data.length > 0 ? (
        <Hint>Attach files by adding a comment.</Hint>
      ) : null}
    </DetailPanel>
  );
}
