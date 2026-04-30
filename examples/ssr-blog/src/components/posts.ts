// Hardcoded posts — the demo focuses on the build pipeline, not data.
export interface Post {
  id: string;
  title: string;
  body: string;
}

export const POSTS: Post[] = [
  {
    id: "first",
    title: "Why SSR is back in fashion",
    body: "First-paint speed and SEO never went anywhere. Hydration tools just got better.",
  },
  {
    id: "ssg-vs-ssr",
    title: "SSG vs SSR — pick on traffic shape",
    body: "If your content rarely changes, prerender it. If it depends on the user, render per request.",
  },
  {
    id: "edge-runtime",
    title: "Edge runtime is the new origin",
    body: "Workers near the user beat round-trips to a single region every time.",
  },
];

export function findPost(id: string): Post | undefined {
  return POSTS.find((p) => p.id === id);
}
