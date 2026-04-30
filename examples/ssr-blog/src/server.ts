"use server";
// Server entry — picked up by the `@zeroship/vite-plugin` (it looks for
// `src/server.ts` by convention) and bundled into `dist/server/index.js`.
//
// IDEAL SHAPE (NOT WHAT THIS FILE DOES TODAY — see "Known limitations" in
// the README):
//
//   import { renderToString } from "react-dom/server";
//   import { App } from "./components/App";
//   import manifest from "./.vite-manifest.json";   // baked in at build time
//
//   export default {
//     async fetch(req: Request) {
//       const url = new URL(req.url);
//       const html = renderToString(<App url={url.pathname} posts={POSTS} />);
//       const jsAsset = manifest["src/entry-client.tsx"].file;  // hashed
//       return new Response(
//         shell.replace("<!--app-html-->", html)
//              .replace("</body>", `<script type="module" src="/${jsAsset}"></script></body>`),
//         { headers: { "content-type": "text/html" } }
//       );
//     }
//   };
//
// What this file ACTUALLY does: exports a single "use server" RPC method
// `ssrRender(url)` that returns the rendered HTML as a JSON-wrapped string.
// The current build pipeline appends its own `export default { fetch }` for
// the RPC bootstrap, so user code can't define `export default` without
// breaking the bundle. See README → "Known limitations" → gap (2).

import { renderToString } from "react-dom/server";
import { createElement } from "react";
import { App } from "./components/App";
import { POSTS } from "./components/posts";

const SHELL = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>ssr-blog — zeroship</title>
  </head>
  <body>
    <div id="root">{{HTML}}</div>
    <script>window.__SSR_PROPS__ = {{PROPS}};</script>
    <!-- Real SSR would inject the hashed JS path here. See README. -->
    <script type="module" src="/src/entry-client.tsx"></script>
  </body>
</html>`;

/**
 * Server-render the React tree for a given URL and return the full HTML.
 * Intended to be called as `ssrRender("/")` or `ssrRender("/post/first")`.
 */
export async function ssrRender(url: string): Promise<string> {
  const html = renderToString(createElement(App, { url, posts: POSTS }));
  const props = JSON.stringify({ url });
  return SHELL.replace("{{HTML}}", html).replace("{{PROPS}}", props);
}
