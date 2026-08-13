import { useState } from "react";
import { Button, Checkbox, Cluster, Field, NumberField, Tag } from "@zeroship/ui";

import { RichText, RichTextEditor, hasText } from "../RichText";
import { addComment, editComment, listComments, setCommentPrivate } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import type { Comment } from "../types";

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

function CommentRow({ comment, onChanged }: { comment: Comment; onChanged: () => void }) {
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
    <li className={`comment ${comment.isPrivate ? "private" : ""}`} id={`comment-${comment.commentNumber}`}>
      <div className="comment-head">
        {/* Comment 0 IS the description -- `bugs.create` writes the description
            as the first comment, the way Bugzilla does, so there is no separate
            description row to render. Presenting it as an anonymous "#0" bubble
            hid that: the one piece of text stating what the bug IS looked like
            the first reply to it. Named rather than restyled, so it reads
            correctly to a screen reader too. */}
        {comment.commentNumber === 0 ? (
          <Tag size="sm">Description</Tag>
        ) : (
          <span className="comment-number">#{comment.commentNumber}</span>
        )}
        <span className="comment-author">{comment.author?.name || comment.author?.handle || comment.authorId}</span>
        <span className="comment-when">{formatDate(comment.created_at)}</span>
        {comment.isPrivate ? <span className="badge private-badge">private</span> : null}
        {comment.workTimeMinutes > 0 ? (
          <span className="comment-worktime">{comment.workTimeMinutes}m logged</span>
        ) : null}
        <Button variant="gray" size="small" disabled={busy} onClick={() => setEditing((v) => !v)}>
          Edit
        </Button>
        <Button variant="gray" size="small" disabled={busy} onClick={() => void togglePrivate()}>
          {comment.isPrivate ? "Make public" : "Make private"}
        </Button>
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
