import { useState } from "react";
import type { Post as PostT } from "./posts";

/** Single-post page. Hydration makes the prev/next buttons interactive. */
export function Post({ post, posts }: { post: PostT; posts: PostT[] }) {
  const [, force] = useState(0);
  const idx = posts.findIndex((p) => p.id === post.id);
  const prev = idx > 0 ? posts[idx - 1] : null;
  const next = idx < posts.length - 1 ? posts[idx + 1] : null;
  // Trick: bump state on hydration so the button enabled-state re-evaluates.
  // (Without hydration these buttons are static HTML.)
  return (
    <main style={styles.wrap}>
      <p><a href="/">← All posts</a></p>
      <h1>{post.title}</h1>
      <p style={styles.body}>{post.body}</p>
      <nav style={styles.nav}>
        <button
          disabled={!prev}
          onClick={() => { if (prev) { window.history.pushState({}, "", `/post/${prev.id}`); force((n) => n + 1); window.location.assign(`/post/${prev.id}`); } }}
          style={styles.btn}
        >
          ← Prev
        </button>
        <button
          disabled={!next}
          onClick={() => { if (next) { window.location.assign(`/post/${next.id}`); } }}
          style={styles.btn}
        >
          Next →
        </button>
      </nav>
    </main>
  );
}

const styles: Record<string, React.CSSProperties> = {
  wrap: { fontFamily: "system-ui, sans-serif", maxWidth: 640, margin: "40px auto", padding: "0 16px" },
  body: { lineHeight: 1.6, color: "#333" },
  nav:  { display: "flex", justifyContent: "space-between", marginTop: 24 },
  btn:  { padding: "8px 14px", border: "1px solid #ccc", background: "#fafafa", borderRadius: 6, cursor: "pointer" },
};
