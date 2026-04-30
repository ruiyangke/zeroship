// Client entry — hydrate the server-rendered HTML in place.
//
// `__SSR_PROPS__` is injected by the server-rendered HTML as a JSON island.
// Hydration replaces the static HTML's React tree with a live one that can
// respond to user interaction (e.g. the prev/next buttons in <Post>).
import { hydrateRoot } from "react-dom/client";
import { App } from "./components/App";
import { POSTS } from "./components/posts";

declare global {
  interface Window { __SSR_PROPS__?: { url: string } }
}

const props = window.__SSR_PROPS__ ?? { url: window.location.pathname };
hydrateRoot(document.getElementById("root")!, <App url={props.url} posts={POSTS} />);
