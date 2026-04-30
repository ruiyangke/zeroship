import { PostList } from "./PostList";
import { Post } from "./Post";
import type { Post as PostT } from "./posts";

export interface AppProps {
  url: string;          // path the server is rendering
  posts: PostT[];       // injected at SSR time, hydrated client-side
}

/** Top-level component. Picks PostList or Post based on `url`. */
export function App({ url, posts }: AppProps) {
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
