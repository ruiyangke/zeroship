import { useEffect, useState, type FormEvent } from "react";
import {
  listNotes,
  addNote,
  deleteNote,
  bumpVisits,
} from "./index";

type Note = { id: string; title: string; body?: string };

export function App() {
  const [notes, setNotes] = useState<Note[]>([]);
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [visits, setVisits] = useState<number | null>(null);

  const refresh = async () => setNotes((await listNotes()) as Note[]);

  useEffect(() => {
    void refresh();
    void Promise.resolve(bumpVisits()).then((v) => setVisits(v as number));
  }, []);

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!title.trim()) return;
    await addNote({ title: title.trim(), body: body.trim() });
    setTitle("");
    setBody("");
    refresh();
  };

  return (
    <div style={{ maxWidth: 640, margin: "2rem auto", fontFamily: "system-ui" }}>
      <h1>zeroship</h1>
      <p style={{ color: "#666" }}>
        {visits === null ? "…" : `visit #${visits}`} — data persists in .zeroship/
      </p>

      <form onSubmit={onSubmit} style={{ marginBottom: "2rem" }}>
        <input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder="title"
          style={{ display: "block", width: "100%", padding: 8, marginBottom: 8 }}
        />
        <textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder="body"
          rows={3}
          style={{ display: "block", width: "100%", padding: 8, marginBottom: 8 }}
        />
        <button type="submit">add note</button>
      </form>

      <ul style={{ listStyle: "none", padding: 0 }}>
        {notes.map((n) => (
          <li
            key={n.id}
            style={{ padding: "0.75rem 0", borderTop: "1px solid #eee" }}
          >
            <div style={{ display: "flex", justifyContent: "space-between" }}>
              <strong>{n.title}</strong>
              <button
                onClick={() => {
                  void Promise.resolve(deleteNote({ id: n.id })).then(refresh);
                }}
              >
                ×
              </button>
            </div>
            {n.body && <div style={{ color: "#555" }}>{n.body}</div>}
          </li>
        ))}
      </ul>
    </div>
  );
}
