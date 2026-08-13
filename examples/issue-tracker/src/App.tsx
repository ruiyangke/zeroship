import { useEffect, useState, type ReactNode } from "react";
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
  | { name: "bug"; id: string }
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
      if (rest[0]) return { name: "bug", id: rest[0] };
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
      return <BugDetailPage id={route.id} />;
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
  const { state: userState } = useAsync(() => currentUser({}), []);

  return (
    <Shell route={route.name} userState={userState}>
      {renderRoute(route)}
    </Shell>
  );
}
