# Builder Live Preview

Date: 2026-05-26

## Decisions

1. Browser preview traffic goes through a builder-origin server-side proxy route:
   `/api/preview/{appId}/{port}/{path*}`.

   The builder runtime resolves the app's sandbox, appends the owner `user_id`,
   and forwards to the sandbox controller's bearer-auth preview proxy:
   `ANY /sandboxes/{id}/preview/{port}/{path*}`. `SANDBOX_TOKEN` stays in the
   builder server runtime and is never serialized into the browser. The iframe
   `src` is a same-origin builder URL.

2. The sandbox dev server starts on demand when `PreviewCanvas` requests
   `sandbox.getLivePreview`.

   The server function resolves the same per-app sandbox used by the builder
   tools, then starts a process through `ZeroshipSandboxBackend.execute` on
   port `5173` if that port is not already listening. For package-based apps it
   runs the app's `dev` script with `--host 0.0.0.0 --port 5173`; for static
   `index.html` sandboxes it falls back to `python3 -m http.server`. The PID and
   logs live under `.zeroship/preview.*`.

## Rejected

Share tokens were not used for the builder iframe path.

The controller share-token handlers are real and useful for public preview
links, but the current local surface is not a clean iframe primitive:

- the minted `share_url` assumes the future `preview-*.preview.zeroship.dev`
  DNS path rather than the local controller path;
- first-hit cookie conversion is guarded as top-level navigation with
  `Sec-Fetch-Mode: navigate` and `Sec-Fetch-Dest: document`, while an iframe
  navigation sends iframe-shaped Fetch Metadata;
- the short-lived token would still appear in the iframe URL on first load.

The builder proxy is simpler for M0, works with the path-shaped controller
proxy that exists today, keeps all long-lived credentials server-side, and
keeps the browser on the builder origin.
