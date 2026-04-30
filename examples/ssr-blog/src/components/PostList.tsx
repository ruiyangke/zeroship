import type { Post } from "./posts";

export function PostList({ posts }: { posts: Post[] }) {
  return (
    <main style={styles.wrap}>
      <h1>ssr-blog</h1>
      <p style={styles.subtitle}>HTML rendered server-side, hydrated client-side.</p>
      <ul style={styles.list}>
        {posts.map((p) => (
          <li key={p.id} style={styles.item}>
            <a href={`/post/${p.id}`}>{p.title}</a>
          </li>
        ))}
      </ul>
    </main>
  );
}

const styles: Record<string, React.CSSProperties> = {
  wrap:     { fontFamily: "system-ui, sans-serif", maxWidth: 640, margin: "40px auto", padding: "0 16px" },
  subtitle: { color: "#666" },
  list:     { listStyle: "none", padding: 0 },
  item:     { padding: "8px 0", borderBottom: "1px solid #eee" },
};
