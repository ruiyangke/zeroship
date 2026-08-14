import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
import { useAuth } from "@zeroship/auth/react";
import { currentUser } from "./api";
import { Shell } from "./components/Shell";
import { useAsync } from "./components/rpc";
import { EmptyState } from "./components/StateViews";
import { BugDetailPage } from "./pages/BugDetail";
import { BugListPage } from "./pages/BugList";
import { DashboardPage } from "./pages/Dashboard";
import { NewBugPage } from "./pages/NewBug";
import { ProductsAdminPage } from "./pages/ProductsAdmin";
import { ReportsPage } from "./pages/Reports";

// Hand-rolled hash router (no react-router-dom in this example). Every
// route is a plain `#/...` fragment; `hashchange` is the only signal we
// listen to, so back/forward and manual URL edits all just work.
export type Route =
  | { name: "bugs" }
  | { name: "bug"; id: string; commentNumber?: number }
  | { name: "new-bug" }
  | { name: "dashboard" }
  | { name: "products" }
  | { name: "reports" }
  | { name: "not-found"; path: string };

export type RouteName = Route["name"];

function parseHash(hash: string): Route {
  const clean = hash.replace(/^#\/?/, "");
  const segments = clean.split("/").filter(Boolean).map(decodeURIComponent);
  const [head, ...rest] = segments;
  switch (head) {
    case undefined:
    case "bugs":
      if (rest[0] === "new") return { name: "new-bug" };
      if (rest[0]) {
        // #/bugs/<id>/c/<n> -- a comment permalink.
        //
        // It used to be a bare "#comment-3". In a hash-routed app that is not
        // a fragment, it REPLACES the route: clicking one left the bug page
        // for "No page here. Nothing is routed at #comment-3." Every comment
        // in every thread carried one.
        //
        // As a route it does what a permalink is for: paste it and you land
        // on the bug, scrolled to the comment.
        const n = rest[1] === "c" ? Number(rest[2]) : NaN;
        return Number.isSafeInteger(n) && n > 0
          ? { name: "bug", id: rest[0], commentNumber: n }
          : { name: "bug", id: rest[0] };
      }
      return { name: "bugs" };
    case "dashboard":
      return { name: "dashboard" };
    case "products":
      return { name: "products" };
    case "reports":
      return { name: "reports" };
    default:
      // NOT a silent fall back to the bug list. An unknown hash used to render
      // Bugs, so a mistyped or stale link answered with a plausible page and
      // never said it had not found the one you asked for -- the worst kind of
      // wrong, because nothing looks wrong.
      return { name: "not-found", path: hash || "#/" };
  }
}

function useHashRoute(): Route {
  const [route, setRoute] = useState<Route>(() => parseHash(window.location.hash));
  useEffect(() => {
    const onHashChange = () => setRoute(parseHash(window.location.hash));
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);
  return route;
}

/**
 * The page for a hash that matches nothing.
 *
 * It names the path it could not resolve. "Page not found" alone leaves you
 * guessing whether the link was wrong or the app is broken; showing the hash
 * back tells you which, and is the difference between a typo you can fix and
 * a bug you would report.
 */
function NotFoundPage({ path }: { path: string }) {
  return (
    <div className="page">
      <EmptyState
        title="No page here"
        hint={
          <>
            Nothing is routed at <code>{path}</code>. Try the{" "}
            <a href="#/bugs">bug list</a>.
          </>
        }
      />
    </div>
  );
}

function renderRoute(route: Route): ReactNode {
  switch (route.name) {
    case "bugs":
      return <BugListPage />;
    case "bug":
      return <BugDetailPage id={route.id} commentNumber={route.commentNumber} />;
    case "new-bug":
      return <NewBugPage />;
    case "dashboard":
      return <DashboardPage />;
    case "products":
      return <ProductsAdminPage />;
    case "reports":
      return <ReportsPage />;
    case "not-found":
      return <NotFoundPage path={route.path} />;
  }
}

export function App() {
  const route = useHashRoute();
  const { state: userState, reload: reloadUser } = useAsync(() => currentUser({}), []);

  // Ask the SERVER again when the browser's session changes.
  //
  // Signing in mints a cookie; it does not tell this app anything. Identity
  // here comes from users.me, fetched once at mount, so the popup would close
  // on a real success and the header would still read "Sign in" -- the flow
  // worked and looked exactly like a flow that had not.
  //
  // Keyed off the user ID rather than the auth object, which is a new
  // reference on every publish and would refetch forever.
  const { user } = useAuth();
  const lastUserId = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    const id = user?.id ?? null;
    if (lastUserId.current === undefined) {
      lastUserId.current = id;
      return;
    }
    if (lastUserId.current === id) return;
    lastUserId.current = id;
    reloadUser();
  }, [user?.id, reloadUser]);

  // ...and remount the PAGE, because refetching identity alone is not enough.
  //
  // Every page runs its own useAsync at mount. Signed out those calls 401 and
  // the page renders "Sign-in required"; signing in refetched users.me, so the
  // header changed and the page did not -- it kept the 401 it was holding and
  // went on advising a reload, which was the only thing that actually worked.
  //
  // A key on the routed content, not a manual reload of each query: pages come
  // and go, and the next one added would have to remember to subscribe. This
  // is one line that cannot be forgotten. Remounting on a session change is
  // cheap because it happens twice a session.
  const sessionKey = user?.id ?? "anon";

  return (
    <Shell route={route.name} userState={userState}>
      <Fragment key={sessionKey}>{renderRoute(route)}</Fragment>
    </Shell>
  );
}
