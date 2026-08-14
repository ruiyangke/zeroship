import { useRef, useState } from "react";
import { Link } from "react-router-dom";
import { Avatar, Button, Checkbox, Cluster, Tag } from "@zeroship/ui";

import { RichText, RichTextEditor, hasText } from "../RichText";
import { addComment, editComment, listAttachments, listComments, setCommentPrivate, uploadAttachment } from "../../api";
import { MAX_ATTACHMENT_BYTES, fileToBase64, formatBytes } from "./attachments";
import { AsyncSection } from "../StateViews";
import { downloadAttachment } from "../../lib/download";
import { errorMessage, useAsync, type AsyncState } from "../rpc";
import type { Activity, Attachment, Comment } from "../types";
import { buildTimeline } from "./timeline";
import { displayValue, fieldLabel, personLabel } from "./activity";
import type { PeopleMap } from "./people";

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

/**
 * How long ago, in words.
 *
 * Every comment carried a full "8/12/2026, 11:52:21 PM". Four of those down a
 * page is a column of digits competing with the prose, and second-level
 * precision answers a question nobody asked. The exact stamp stays in the
 * title attribute, where it costs nothing until wanted.
 */
function timeAgo(ms: number): string {
  const seconds = Math.round((Date.now() - ms) / 1000);
  if (seconds < 60) return "just now";
  const units: [number, string][] = [
    [60, "minute"],
    [3600, "hour"],
    [86400, "day"],
    [604800, "week"],
    [2592000, "month"],
    [31536000, "year"],
  ];
  let unit = units[0];
  for (const candidate of units) if (seconds >= candidate[0]) unit = candidate;
  const value = Math.floor(seconds / unit[0]);
  return value + " " + unit[1] + (value === 1 ? "" : "s") + " ago";
}

/** Initials for the avatar fallback: "Alice Dev" becomes "AD". */
function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  return (parts[0][0] + (parts[1]?.[0] ?? "")).toUpperCase();
}

function CommentRow({
  comment,
  onChanged,
  readOnly = false,
  attachments = [],
}: {
  comment: Comment;
  onChanged: () => void;
  readOnly?: boolean;
  /** The files that arrived with THIS comment. */
  attachments?: Attachment[];
}) {
  const author = comment.author?.name || comment.author?.handle || comment.authorId;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(comment.body);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await editComment({ id: comment.id, body: draft });
      setEditing(false);
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  /**
   * Leave the editor without saving, and forget what was typed.
   *
   * There was no way out but the button labelled "Edit", and taking it kept
   * the draft: reopening showed your abandoned changes sitting there looking
   * like the comment's text. Discarding is the whole point of cancelling, so
   * it is one function and both exits call it.
   */
  const cancelEditing = () => {
    setDraft(comment.body);
    setEditing(false);
    setError(null);
  };

  const togglePrivate = async () => {
    setBusy(true);
    setError(null);
    try {
      await setCommentPrivate({ id: comment.id, isPrivate: !comment.isPrivate });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <li
      className={[
        "comment",
        comment.isPrivate ? "private" : "",
        comment.commentNumber === 0 ? "is-description" : "",
      ]
        .filter(Boolean)
        .join(" ")}
      id={`comment-${comment.commentNumber}`}
    >
      {/* Identity leads. Previously the loudest things in this row were the
          Edit and Make private buttons, which sat at full weight beside a
          timestamp at full precision, so the eye met the controls before the
          author. The avatar and name come first now and the actions wait for
          a hover or a keyboard focus. */}
      <Avatar size="sm" fallback={initials(author)} aria-hidden="true" />
      <div className="comment-body-column">
      <div className="comment-head">
        {/* Comment 0 IS the description -- `bugs.create` writes the description
            as the first comment, the way Bugzilla does, so there is no separate
            description row to render. Presenting it as an anonymous "#0" bubble
            hid that: the one piece of text stating what the bug IS looked like
            the first reply to it. Named rather than restyled, so it reads
            correctly to a screen reader too. */}
        <span className="comment-author">{author}</span>
        {comment.commentNumber === 0 ? (
          <Tag size="sm">Description</Tag>
        ) : null}
        <span className="comment-when" title={formatDate(comment.created_at)}>
          {timeAgo(comment.created_at)}
        </span>
        {comment.isPrivate ? <span className="badge private-badge">private</span> : null}
        {/* A ROUTE, not a bare fragment. "#comment-3" reads like an anchor
            and is one in a normal page, but this app routes on the hash, so
            clicking it replaced the route and landed on "No page here.
            Nothing is routed at #comment-3" -- from every comment, in every
            thread. The <li> keeps its id so the page can scroll to it. */}
        {comment.commentNumber > 0 ? (
          <Link
            className="comment-number"
            to={`/bugs/${comment.bugId}/c/${comment.commentNumber}`}
          >
            #{comment.commentNumber}
          </Link>
        ) : null}
        {readOnly ? null : (
        <span className="comment-actions">
          <Button
            variant="plain"
            size="small"
            disabled={busy}
            aria-label={`Edit comment ${comment.commentNumber}`}
            onClick={() => (editing ? cancelEditing() : setEditing(true))}
          >
            Edit
          </Button>
          <Button
            variant="plain"
            size="small"
            disabled={busy}
            aria-label={
              (comment.isPrivate ? "Make comment public " : "Make comment private ") +
              comment.commentNumber
            }
            onClick={() => void togglePrivate()}
          >
            {comment.isPrivate ? "Make public" : "Make private"}
          </Button>
        </span>
        )}
      </div>
      {editing ? (
        <div className="comment-edit">
          <RichTextEditor value={draft} onChange={setDraft} ariaLabel="Edit comment" />
          <Cluster gap={2} align="center">
            <Button variant="filled" size="small" disabled={busy} onClick={() => void save()}>
              Save
            </Button>
            <Button
              variant="plain"
              size="small"
              disabled={busy}
              aria-label={`Cancel editing comment ${comment.commentNumber}`}
              onClick={cancelEditing}
            >
              Cancel
            </Button>
          </Cluster>
        </div>
      ) : (
        <div className="comment-body">
          {/* Rendered THROUGH tiptap, not with dangerouslySetInnerHTML. The
              body is arbitrary text over RPC, so parsing it back through the
              schema that writes it is what keeps a crafted comment from
              running in every reader's browser. */}
          <RichText markdown={comment.body} />
        </div>
      )}
      {attachments.length > 0 ? (
        <ul className="comment-attachments">
          {attachments.map((file) => (
            <li key={file.id}>
              {/* A button. It was an anchor to
                  "/bugs/<id>?attachment=<fileId>", which is not a deep link
                  in a hash-routed app: the hash splits on "/", so the query
                  rode inside the bug id and the page asked the server for a
                  bug that cannot exist. Nothing read the parameter either.
                  Attachments arrive over RPC as base64, so there is no URL to
                  point at -- downloading IS the action. */}
              <button
                type="button"
                className="comment-attachment"
                onClick={() => {
                  downloadAttachment(file.id, file.filename).catch((err: unknown) =>
                    setError(errorMessage(err)),
                  );
                }}
              >
                {file.filename}
              </button>
              <span className="dim small"> {formatBytes(file.sizeBytes)}</span>
            </li>
          ))}
        </ul>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
      </div>
    </li>
  );
}

function NewCommentForm({ bugId, onAdded }: { bugId: string; onAdded: () => void }) {
  const [body, setBody] = useState("");
  const [isPrivate, setIsPrivate] = useState(false);
  const fileRef = useRef<HTMLInputElement | null>(null);
  // Named separately from the input so the chosen files can be listed back.
  // A bare file input shows one filename and silently hides the rest.
  const [fileNames, setFileNames] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    // An empty tiptap document is "<p></p>", not "". Guarding on the markup
    // would let an empty comment through and post a blank bubble.
    const files = Array.from(fileRef.current?.files ?? []);
    if (!hasText(body) && files.length === 0) return;
    const tooBig = files.find((file) => file.size > MAX_ATTACHMENT_BYTES);
    if (tooBig) {
      setError(
        `${tooBig.name} is ${formatBytes(tooBig.size)}; the server caps attachments at ${formatBytes(MAX_ATTACHMENT_BYTES)}.`,
      );
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const comment = await addComment({ bugId, body, isPrivate });
      // The comment first, then its files -- an attachment names the comment
      // it arrived with, so the comment has to exist to be named. If a file
      // fails here the comment still stands, which is the right way round:
      // the sentence explaining the file is worth more than the file.
      for (const file of files) {
        await uploadAttachment({
          bugId,
          commentId: comment.id,
          filename: file.name,
          contentBase64: await fileToBase64(file),
          contentType: file.type || "application/octet-stream",
        });
      }
      setBody("");
      setIsPrivate(false);
      if (fileRef.current) fileRef.current.value = "";
      setFileNames([]);
      onAdded();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="new-comment">
      <RichTextEditor
        value={body}
        onChange={setBody}
        placeholder="Add a comment..."
        ariaLabel="Add a comment"
        collapsible
      />
      {/* A Cluster, not a bare flex row. Checkbox renders its own label
          BELOW its box in this row and it landed on top of the next field's
          label -- two labels overlapping reads as a rendering fault. */}
      <Cluster gap={3} align="center" className="new-comment-row" justify="between">
        {/* Checkbox owns its own label, so the wrapping <label> that used to
            associate a bare input is gone rather than nested inside it. */}
        <Checkbox
          checked={isPrivate}
          onCheckedChange={(next) => setIsPrivate(next === true)}
          label="Private"
        />
        {/* hasText, not body.trim(). The body is HTML: clearing the editor
            leaves "<p></p>", which trims to a NON-empty string, so the button
            enabled itself while submit() -- which already guarded on hasText --
            refused the post. A live control that does nothing when clicked. */}
        {/* Attaching is part of commenting, not a separate errand on the
            other side of the page. A file almost always needs a sentence
            saying what it is; putting the picker here means the sentence and
            the file arrive together and stay together. */}
        {/* A real button, driving a hidden input.
            The native control rendered "Choose Files | No fi...osen" -- browser
            chrome, truncated, in the middle of a row of design-system
            controls. The input keeps its label and stays in the DOM (that is
            what makes it operable at all, and what a file picker must be), it
            simply stops being the thing on screen. */}
        <Button
          variant="gray"
          size="small"
          onClick={() => fileRef.current?.click()}
        >
          Attach files
        </Button>
        <input
          ref={fileRef}
          type="file"
          multiple
          className="visually-hidden"
          aria-label="Attach files to this comment"
          onChange={(event) =>
            setFileNames(Array.from(event.target.files ?? []).map((file) => file.name))
          }
        />
        <Button
          variant="filled"
          size="small"
          // A file with no words is still worth posting, so the guard is
          // "nothing at all" rather than "no text".
          disabled={busy || (!hasText(body) && fileNames.length === 0)}
          onClick={() => void submit()}
        >
          Comment
        </Button>
      </Cluster>
      {fileNames.length > 0 ? (
        <p className="comment-attach-queue small">
          {fileNames.length === 1 ? "Attaching" : "Attaching " + fileNames.length + " files:"}{" "}
          {fileNames.join(", ")}
        </p>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

export function CommentsPanel({
  bugId,
  readOnly = false,
  attachments,
  onAttachmentsChanged,
  activities = [],
  people = {},
  labels = {},
}: {
  bugId: string;
  /** Field changes, shown inline the way GitHub and Linear do. */
  activities?: readonly Activity[];
  people?: PeopleMap;
  /**
   * Ids to the names they stand for, shared with the History tab.
   *
   * The same map feeds both views on purpose: two resolvers is how one of
   * them ends up printing a raw id after a field is added to the log.
   */
  labels?: Record<string, string>;
  /** No identity: the thread is readable, the controls are not offered. */
  readOnly?: boolean;
  /** Owned by the page, because the files panel lists the same rows. */
  attachments: AsyncState<Attachment[]>;
  onAttachmentsChanged: () => void;
}) {
  const { state, reload } = useAsync(() => listComments({ bugId }), [bugId]);
  // Fetched once for the thread and handed out per comment, rather than each
  // row asking for its own -- one request either way, and the grouping is a
  // property of the thread, not of any single comment.
  const filesByComment = new Map<string, Attachment[]>();
  if (attachments.status === "ready") {
    for (const file of attachments.data) {
      const key = file.commentId ?? "";
      if (!key) continue;
      const list = filesByComment.get(key) ?? [];
      list.push(file);
      filesByComment.set(key, list);
    }
  }

  return (
    <section className="comments-panel">
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading comments..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No comments yet."
      >
        {(comments) => (
          <ul className="comment-list">
            {buildTimeline(comments, activities).map((item, index) =>
              item.kind === "comment" ? (
                <CommentRow
                  key={item.comment.id}
                  comment={item.comment}
                  onChanged={() => {
                    reload();
                    onAttachmentsChanged();
                  }}
                  readOnly={readOnly}
                  attachments={filesByComment.get(item.comment.id) ?? []}
                />
              ) : (
                <li key={"event-" + index} className="timeline-event">
                  <span className="timeline-dot" aria-hidden="true" />
                  <p className="timeline-event-text">
                    <span className="timeline-actor">{personLabel(item.actorId, people)}</span>{" "}
                    {item.changes.map((change, i) => (
                      <span key={i}>
                        {i > 0 ? ", " : ""}
                        set <span className="timeline-field">{fieldLabel(change.fieldName)}</span> to{" "}
                        <span className="timeline-value">{displayValue(change.newValue, labels) || "nothing"}</span>
                      </span>
                    ))}
                    <span className="timeline-when" title={formatDate(item.at)}>
                      {" "}
                      {timeAgo(item.at)}
                    </span>
                  </p>
                </li>
              ),
            )}
          </ul>
        )}
      </AsyncSection>
      {readOnly ? null : (
        <NewCommentForm
          bugId={bugId}
          onAdded={() => {
            reload();
            onAttachmentsChanged();
          }}
        />
      )}
    </section>
  );
}
