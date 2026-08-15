import { useRef, useState } from "react";
import { Link } from "react-router-dom";
import { Avatar, Button, Checkbox, Cluster, Tag } from "@zeroship/ui";

import { RichText, RichTextEditor, hasText } from "../RichText";
import { addComment, editComment, setCommentPrivate, uploadAttachment } from "../../api";
import { MAX_ATTACHMENT_BYTES, fileToBase64, formatBytes } from "./attachments";
import { AsyncSection } from "../StateViews";
import { downloadAttachment } from "../../lib/download";
import { errorMessage } from "../rpc";
import { useAppMutation, useAttachments, useComments } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
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
  readOnly = false,
  attachments = [],
}: {
  comment: Comment;
  readOnly?: boolean;
  /** The files that arrived with THIS comment. */
  attachments?: Attachment[];
}) {
  const author = comment.author?.name || comment.author?.handle || comment.authorId;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(comment.body);
  const [error, setError] = useState<string | null>(null);

  // Editing a comment changes the thread, and the thread's key is not the only
  // thing it makes stale -- `commentChanged` names the issue detail as well.
  const edit = useAppMutation(
    (body: string) => editComment({ id: comment.id, body }),
    () => invalidatedBy.commentChanged(comment.issueId),
  );
  const setPrivate = useAppMutation(
    (isPrivate: boolean) => setCommentPrivate({ id: comment.id, isPrivate }),
    () => invalidatedBy.commentChanged(comment.issueId),
  );
  const busy = edit.isPending || setPrivate.isPending;

  const save = async () => {
    setError(null);
    try {
      await edit.mutateAsync(draft);
      // Leaving the editor is still this row's decision; only the refresh of
      // the thread and the issue moved out of it.
      setEditing(false);
    } catch (err) {
      setError(errorMessage(err));
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
    setError(null);
    try {
      await setPrivate.mutateAsync(!comment.isPrivate);
    } catch (err) {
      setError(errorMessage(err));
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
        {/* Comment 0 IS the description -- `issues.create` writes the description
            as the first comment, the way Bugzilla does, so there is no separate
            description row to render. Presenting it as an anonymous "#0" bubble
            hid that: the one piece of text stating what the issue IS looked like
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
            to={`/issues/${comment.issueId}/c/${comment.commentNumber}`}
          >
            #{comment.commentNumber}
          </Link>
        ) : null}
        {readOnly ? null : (
        <span className="comment-actions">
          <Button
            variant="plain"
            size="sm"
            disabled={busy}
            aria-label={`Edit comment ${comment.commentNumber}`}
            onClick={() => (editing ? cancelEditing() : setEditing(true))}
          >
            Edit
          </Button>
          <Button
            variant="plain"
            size="sm"
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
            <Button variant="filled" size="sm" disabled={busy} onClick={() => void save()}>
              Save
            </Button>
            <Button
              variant="plain"
              size="sm"
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
                  "/issues/<id>?attachment=<fileId>", which is not a deep link
                  in a hash-routed app: the hash splits on "/", so the query
                  rode inside the issue id and the page asked the server for a
                  issue that cannot exist. Nothing read the parameter either.
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

function NewCommentForm({ issueId }: { issueId: string }) {
  const [body, setBody] = useState("");
  const [isPrivate, setIsPrivate] = useState(false);
  const fileRef = useRef<HTMLInputElement | null>(null);
  // Named separately from the input so the chosen files can be listed back.
  // A bare file input shows one filename and silently hides the rest.
  const [fileNames, setFileNames] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  // Posting can change two lists and one count, and it says so once here.
  // `commentChanged` carries the issue detail because `commentCount` lives on
  // the issue -- without it the new comment appears while the count beside it
  // still reads one fewer. `attachmentChanged` covers the files this post
  // brought with it, which the roll-up panel renders from the same key.
  const post = useAppMutation(
    // The draft travels in as an ARGUMENT rather than being read off the
    // closure: the composer re-renders on every keystroke, and a mutation that
    // reads its input from whichever render it was defined in is the kind of
    // thing that works until it does not.
    async ({ body, isPrivate, files }: { body: string; isPrivate: boolean; files: File[] }) => {
      const comment = await addComment({ issueId, body, isPrivate });
      // The comment first, then its files -- an attachment names the comment
      // it arrived with, so the comment has to exist to be named. If a file
      // fails here the comment still stands, which is the right way round:
      // the sentence explaining the file is worth more than the file.
      for (const file of files) {
        await uploadAttachment({
          issueId,
          commentId: comment.id,
          filename: file.name,
          contentBase64: await fileToBase64(file),
          contentType: file.type || "application/octet-stream",
        });
      }
    },
    () => [
      ...invalidatedBy.commentChanged(issueId),
      ...invalidatedBy.attachmentChanged(issueId),
    ],
  );
  const busy = post.isPending;

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
    setError(null);
    try {
      await post.mutateAsync({ body, isPrivate, files });
      // Emptying the composer is the form's own job. The invalidation has
      // already been awaited by this point, so the cleared box and the posted
      // comment appear together rather than racing.
      setBody("");
      setIsPrivate(false);
      if (fileRef.current) fileRef.current.value = "";
      setFileNames([]);
    } catch (err) {
      setError(errorMessage(err));
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
          size="sm"
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
          size="sm"
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
  issueId,
  readOnly = false,
  activities = [],
  people = {},
  labels = {},
}: {
  issueId: string;
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
}) {
  const commentsQ = useComments(issueId);
  // Asked for by KEY, not handed down. The files panel asks the same question
  // under the same key, so the two views share one request and one answer --
  // which is what the page-owned prop used to buy, at the cost of a callback
  // each side had to remember. An upload now invalidates the key and both
  // views follow; before, the thread refreshed and the roll-up sat there
  // saying "No files yet" beside the file it was denying.
  const attachmentsQ = useAttachments(issueId);
  // Grouped once for the thread and handed out per comment, rather than each
  // row asking for its own -- the grouping is a property of the thread, not of
  // any single comment.
  const filesByComment = new Map<string, Attachment[]>();
  for (const file of attachmentsQ.data ?? []) {
    const key = file.commentId ?? "";
    if (!key) continue;
    const list = filesByComment.get(key) ?? [];
    list.push(file);
    filesByComment.set(key, list);
  }

  return (
    <section className="comments-panel">
      <AsyncSection
        query={commentsQ}
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
      {readOnly ? null : <NewCommentForm issueId={issueId} />}
    </section>
  );
}
