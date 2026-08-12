import { useEffect, useState, type ReactNode } from "react";
import { currentUser } from "./api";
import { Nav } from "./components/Nav";
import { useAsync } from "./components/rpc";
import { AdvancedSearchPage } from "./pages/AdvancedSearch";
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
  | { name: "search" }
  | { name: "dashboard" }
  | { name: "products" }
  | { name: "reports" };

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
    case "search":
      return { name: "search" };
    case "dashboard":
      return { name: "dashboard" };
    case "products":
      return { name: "products" };
    case "reports":
      return { name: "reports" };
    default:
      return { name: "bugs" };
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

function renderRoute(route: Route): ReactNode {
  switch (route.name) {
    case "bugs":
      return <BugListPage />;
    case "bug":
      return <BugDetailPage id={route.id} />;
    case "new-bug":
      return <NewBugPage />;
    case "search":
      return <AdvancedSearchPage />;
    case "dashboard":
      return <DashboardPage />;
    case "products":
      return <ProductsAdminPage />;
    case "reports":
      return <ReportsPage />;
  }
}

export function App() {
  const route = useHashRoute();
  const { state: userState } = useAsync(() => currentUser({}), []);

  return (
    <div className="shell">
      <Nav route={route.name} userState={userState} />
      <main className="content">{renderRoute(route)}</main>
    </div>
  );
}
