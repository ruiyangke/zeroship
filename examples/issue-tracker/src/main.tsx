import React from "react";
import { createRoot } from "react-dom/client";
import { AuthProvider } from "@zeroship/auth/react";
import { BrowserRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ThemeProvider } from "@zeroship/ui";
// The component library is headless. This app owns its complete visual layer.
import "./theme/index.css";
import { App } from "./App";
// App-specific composition follows the component theme.
import "./styles.css";
import { isUnauthenticated } from "./components/rpc";

/**
 * The query cache, and the one policy decision worth making here.
 *
 * The app used a bespoke `useAsync` with, in its own words, "no cache, no
 * refetch-on-focus, no request de-duplication". The absent de-duplication was
 * the expensive part: six components asked `users.me` independently, so one
 * visit to the issue list fired it FOUR times, and `listProducts` had five
 * callers. Keying a query fixes that structurally instead of asking every page
 * to remember.
 *
 * RETRY IS THE INTERESTING SETTING. Most of this app's procedures are
 * `auth: "user"`, so a signed-out visitor gets a 401 from a large fraction of
 * them -- and a 401 is an ANSWER, not a failure to reach the server. Retrying
 * it doubles the requests on every anonymous page load and, worse, delays the
 * moment the UI learns there is nobody, which is exactly the window where a
 * control it should not offer is still on screen. So: retry transport
 * failures once, never retry an authentication answer.
 *
 * `refetchOnWindowFocus` stays off to match the sibling examples
 * (`examples/db-todos`). This app shows tables of a hundred rows; refetching
 * them because someone alt-tabbed is a cost with no reader benefit.
 */
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      refetchOnWindowFocus: false,
      retry: (failureCount, error) => !isUnauthenticated(error) && failureCount < 1,
    },
  },
});

const el = document.getElementById("root");
if (!el) throw new Error("missing #root");
createRoot(el).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
    <ThemeProvider>
      {/* The app had no way in. Every signed-out state said "sign in through
          the platform and reload" and nothing anywhere performed a sign-in --
          the only route to an identity was minting the dev session cookie by
          hand, which is what the e2e helper does and what no person would.
          The platform ships the flow; the app simply never called it. */}
      <AuthProvider>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </AuthProvider>
    </ThemeProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
