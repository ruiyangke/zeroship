import { useState } from "react";
import { Avatar, Button, Checkbox, Cluster, Field, NumberField, Tag } from "@zeroship/ui";

import { RichText, RichTextEditor, hasText } from "../RichText";
import { addComment, editComment, listComments, setCommentPrivate } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import type { Comment } from "../types";

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

function CommentRow({ comment, onChanged }: { comment: Comment; onChanged: () => void }) {
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
        {comment.workTimeMinutes > 0 ? (
          <span className="comment-worktime">{comment.workTimeMinutes}m logged</span>
        ) : null}
        {/* The permalink the "#3" used to be. It was a bare label doing
            nothing; anchors already exist on the <li>, so it may as well be
            the thing you can copy. */}
        {comment.commentNumber > 0 ? (
          <a className="comment-number" href={`#comment-${comment.commentNumber}`}>
            #{comment.commentNumber}
          </a>
        ) : null}
        <span className="comment-actions">
          <Button
            variant="plain"
            size="small"
            disabled={busy}
            aria-label={`Edit comment ${comment.commentNumber}`}
            onClick={() => setEditing((v) => !v)}
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
      </div>
      {editing ? (
        <div className="comment-edit">
          <RichTextEditor value={draft} onChange={setDraft} ariaLabel="Edit comment" />
          <Button variant="filled" size="small" disabled={busy} onClick={() => void save()}>
            Save
          </Button>
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
      {error ? <p className="field-error">{error}</p> : null}
      </div>
    </li>
  );
}

function NewCommentForm({ bugId, onAdded }: { bugId: string; onAdded: () => void }) {
  const [body, setBody] = useState("");
  const [isPrivate, setIsPrivate] = useState(false);
  const [workTimeMinutes, setWorkTimeMinutes] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    // An empty tiptap document is "<p></p>", not "". Guarding on the markup
    // would let an empty comment through and post a blank bubble.
    if (!hasText(body)) return;
    setBusy(true);
    setError(null);
    try {
      await addComment({ bugId, body, isPrivate, workTimeMinutes });
      setBody("");
      setIsPrivate(false);
      setWorkTimeMinutes(0);
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
      <Cluster gap={3} align="center" className="new-comment-row">
        {/* Checkbox owns its own label, so the wrapping <label> that used to
            associate a bare input is gone rather than nested inside it. */}
        <Checkbox
          checked={isPrivate}
          onCheckedChange={(next) => setIsPrivate(next === true)}
          label="Private"
        />
        <Field orientation="horizontal">
          <Field.Label>Work time (min)</Field.Label>
          <NumberField
            min={0}
            value={workTimeMinutes}
            onValueChange={(next) => setWorkTimeMinutes(next ?? 0)}
          />
        </Field>
        {/* hasText, not body.trim(). The body is HTML: clearing the editor
            leaves "<p></p>", which trims to a NON-empty string, so the button
            enabled itself while submit() -- which already guarded on hasText --
            refused the post. A live control that does nothing when clicked. */}
        <Button
          variant="filled"
          size="small"
          disabled={busy || !hasText(body)}
          onClick={() => void submit()}
        >
          Comment
        </Button>
      </Cluster>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

export function CommentsPanel({ bugId }: { bugId: string }) {
  const { state, reload } = useAsync(() => listComments({ bugId }), [bugId]);

  return (
    <section className="comments-panel">
      <h3>Comments</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading comments..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No comments yet."
      >
        {(comments) => (
          <ul className="comment-list">
            {comments.map((comment) => (
              <CommentRow key={comment.id} comment={comment} onChanged={reload} />
            ))}
          </ul>
        )}
      </AsyncSection>
      <NewCommentForm bugId={bugId} onAdded={reload} />
    </section>
  );
}
