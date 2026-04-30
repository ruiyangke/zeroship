import { PostList } from "./PostList";
import { Post } from "./Post";
import { listPosts } from "../server";

export interface AppProps {
  url: string;          // path the server is rendering
}

/** Top-level component. Calls `listPosts.useQuery()` — works on both
 *  the SSR side (synchronous, prefetched into QueryClient) and the
 *  browser side (HTTP /_zs/v1/listPosts via the client SDK). */
export function App({ url }: AppProps) {
  // The cast is needed because the vite-plugin monkey-patches
  // hooks on at build time; TS doesn't see them statically.
  const lp = listPosts as typeof listPosts & {
    useQuery: (input?: undefined) => { data?: unknown[] };
  };
  const { data } = lp.useQuery();
  const posts = (data as { id: string; title: string; body: string }[] | undefined) ?? [];

  if (url === "/" || url === "") {
    return <PostList posts={posts} />;
  }
  const m = url.match(/^\/post\/([^/]+)\/?$/);
  if (m) {
    const post = posts.find((p) => p.id === m[1]);
    if (!post) return <NotFound />;
    return <Post post={post} posts={posts} />;
  }
  return <NotFound />;
}

function NotFound() {
  return (
    <main style={{ fontFamily: "system-ui, sans-serif", padding: 40 }}>
      <h1>404</h1>
      <p>No such post.</p>
      <a href="/">← Home</a>
    </main>
  );
}
