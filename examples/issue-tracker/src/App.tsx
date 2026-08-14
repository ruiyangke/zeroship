import { Fragment, useEffect, useRef } from "react";
import { useAuth } from "@zeroship/auth/react";
import { Link, Route, Routes, useLocation, useParams } from "react-router-dom";
import { currentUser } from "./api";
import { Shell } from "./components/Shell";
import { useAsync } from "./components/rpc";
import { EmptyState } from "./components/StateViews";
import { IssueDetailPage } from "./pages/IssueDetail";
import { IssueListPage } from "./pages/IssueList";
import { DashboardPage } from "./pages/Dashboard";
import { NewIssuePage } from "./pages/NewIssue";
import { ProductsAdminPage } from "./pages/ProductsAdmin";
import { ReportsPage } from "./pages/Reports";

/**
 * Routes, declared. This app hand-rolled a router twice -- first on the hash,
 * then on the History API -- and both versions shipped bugs that a real router
 * makes unspellable: a comment permalink written as "#comment-3" replaced the
 * route instead of jumping within the page, and a link carrying a query had it
 * swallowed into an id because the parser split the whole URL on "/".
 *
 * react-router-dom is already this repo's router (examples/hr-system) and it
 * is pinned in the workspace catalog, so this is the established dependency
 * rather than a new one.
 */

/** The issue page, for both /issues/:id and its /c/:n permalink. */
function IssueDetailRoute() {
  const { id, n } = useParams();
  const parsed = n === undefined ? NaN : Number(n);
  return (
    <IssueDetailPage
      id={id ?? ""}
      commentNumber={Number.isSafeInteger(parsed) && parsed > 0 ? parsed : undefined}
    />
  );
}

/**
 * The page for a path that matches nothing.
 *
 * It names the path it could not resolve. "Page not found" alone leaves you
 * guessing whether the link was wrong or the app is broken; showing the path
 * back tells you which, and is the difference between a typo you can fix and
 * a bug you would report.
 *
 * NOT a redirect to the issue list. An unknown path used to render Issues, so a
 * mistyped or stale link answered with a plausible page and never said it had
 * not found the one you asked for -- the worst kind of wrong, because nothing
 * looks wrong.
 */
function NotFoundPage() {
  const { pathname } = useLocation();
  return (
    <div className="page">
      <EmptyState
        title="No page here"
        hint={
          <>
            Nothing is routed at <code>{pathname}</code>. Try the{" "}
            <Link to="/issues">issue list</Link>.
          </>
        }
      />
    </div>
  );
}

export function App() {
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
    <Shell userState={userState}>
      <Fragment key={sessionKey}>
        <Routes>
          <Route path="/" element={<IssueListPage />} />
          <Route path="/issues" element={<IssueListPage />} />
          <Route path="/issues/new" element={<NewIssuePage />} />
          <Route path="/issues/:id" element={<IssueDetailRoute />} />
          {/* The comment permalink. A route, so pasting it lands on the issue
              scrolled to that comment. */}
          <Route path="/issues/:id/c/:n" element={<IssueDetailRoute />} />
          <Route path="/dashboard" element={<DashboardPage />} />
          <Route path="/products" element={<ProductsAdminPage />} />
          <Route path="/reports" element={<ReportsPage />} />
          <Route path="*" element={<NotFoundPage />} />
        </Routes>
      </Fragment>
    </Shell>
  );
}
