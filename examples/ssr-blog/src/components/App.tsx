import { PostList } from "./PostList";
import { Post } from "./Post";
import { useQuery } from "@tanstack/react-query";
import { listPosts } from "../server";

export interface AppProps {
  url: string;          // path the server is rendering
}

export const LIST_POSTS_QUERY_KEY = ["posts", "list"] as const;

/** Top-level component. React Query wraps the typed RPC caller directly. */
export function App({ url }: AppProps) {
  const { data: posts = [] } = useQuery({
    queryKey: LIST_POSTS_QUERY_KEY,
    queryFn: () => listPosts(undefined),
  });

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
