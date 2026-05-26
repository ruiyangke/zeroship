// Client entry — hydrate the server-rendered HTML in place.
//
// `__SSR_PROPS__` is injected by the server-rendered HTML as a JSON island.
// Hydration replaces the static HTML's React tree with a live one that can
// respond to user interaction (e.g. the prev/next buttons in <Post>).
import { hydrateRoot } from "react-dom/client";
import { HydrationBoundary, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ComponentType, ReactNode } from "react";
import { App } from "./components/App";

declare global {
  interface Window { __SSR_PROPS__?: { url: string; dehydrated?: unknown } }
}

const props = window.__SSR_PROPS__;

if (props) {
  const queryClient = new QueryClient();
  const RQHydrationBoundary = HydrationBoundary as unknown as ComponentType<{
    state?: unknown;
    children?: ReactNode;
  }>;

  hydrateRoot(
    document.getElementById("root")!,
    <QueryClientProvider client={queryClient}>
      <RQHydrationBoundary state={props.dehydrated}>
        <App url={props.url} />
      </RQHydrationBoundary>
    </QueryClientProvider>,
  );
}
