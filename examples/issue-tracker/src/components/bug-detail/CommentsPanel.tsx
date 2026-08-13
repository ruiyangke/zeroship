import { useState } from "react";
import { Button, Checkbox, Field, NumberField } from "@zeroship/ui";
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
        <span className="comment-number">#{comment.commentNumber}</span>
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
          <textarea
            aria-label="Edit comment"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            rows={4}
          />
          <Button variant="filled" size="small" disabled={busy} onClick={() => void save()}>
            Save
          </Button>
        </div>
      ) : (
        <p className="comment-body">{comment.body}</p>
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
    if (!body.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await addComment({ bugId, body: body.trim(), isPrivate, workTimeMinutes });
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
      <textarea
        aria-label="Add a comment"
        placeholder="Add a comment..."
        value={body}
        onChange={(e) => setBody(e.target.value)}
        rows={4}
      />
      <div className="new-comment-row">
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
        <Button variant="filled" size="small" disabled={busy || !body.trim()} onClick={() => void submit()}>
          Comment
        </Button>
      </div>
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
