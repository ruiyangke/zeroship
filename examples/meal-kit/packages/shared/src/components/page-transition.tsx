import { useLayoutEffect, useRef, type ReactNode } from "react";
import { useLocation, useNavigationType } from "react-router-dom";
import { ContentTransition } from "@gather/meal-kit/components/content-transition";

const scrollPositions = new Map<string, { x: number; y: number }>();
let lastVisitedPage: string | undefined;

function usePageNavigation(page: string) {
  const location = useLocation();
  const navigation = useNavigationType();
  const previous = useRef({ page: lastVisitedPage ?? page, hash: "" });

  useLayoutEffect(() => {
    const mode = history.scrollRestoration;
    history.scrollRestoration = "manual";
    return () => {
      history.scrollRestoration = mode;
    };
  }, []);

  useLayoutEffect(() => {
    const changed =
      page !== previous.current.page || location.hash !== previous.current.hash;
    previous.current = { page, hash: location.hash };
    lastVisitedPage = page;
    let observer: ResizeObserver | undefined;
    let anchorObserver: MutationObserver | undefined;
    let restoring = false;
    const stopRestoring = () => {
      restoring = false;
      observer?.disconnect();
      anchorObserver?.disconnect();
    };
    const record = () => {
      if (restoring) return;
      scrollPositions.delete(location.key);
      scrollPositions.set(location.key, { x: scrollX, y: scrollY });
      if (scrollPositions.size > 200)
        scrollPositions.delete(scrollPositions.keys().next().value!);
    };
    if (changed) {
      const anchorId = /^#(?:choose-meals|step-\d+)$/.test(location.hash)
        ? location.hash.slice(1)
        : null;
      const anchor = anchorId ? document.getElementById(anchorId) : null;
      (anchor ?? document.getElementById("main"))?.focus({
        preventScroll: true,
      });
      const saved = navigation === "POP" && scrollPositions.get(location.key);
      if (saved) {
        restoring = true;
        const restore = () => {
          window.scrollTo({ left: saved.x, top: saved.y, behavior: "instant" });
          if (Math.abs(scrollY - saved.y) < 1) stopRestoring();
        };
        observer = new ResizeObserver(restore);
        observer.observe(document.documentElement);
        restore();
      } else if (anchorId) {
        const reveal = () => {
          const target = document.getElementById(anchorId);
          if (!target) return false;
          target.focus({ preventScroll: true });
          target.scrollIntoView({ behavior: "instant" });
          stopRestoring();
          return true;
        };
        if (!reveal()) {
          restoring = true;
          anchorObserver = new MutationObserver(reveal);
          anchorObserver.observe(document.getElementById("main")!, {
            childList: true,
            subtree: true,
          });
        }
      } else {
        window.scrollTo({ top: 0, left: 0, behavior: "instant" });
      }
    }
    window.addEventListener("scroll", record, { passive: true });
    record();
    const interruptions = [
      "wheel",
      "touchstart",
      "pointerdown",
      "keydown",
    ] as const;
    for (const event of interruptions)
      window.addEventListener(event, stopRestoring, { passive: true });
    return () => {
      stopRestoring();
      window.removeEventListener("scroll", record);
      for (const event of interruptions)
        window.removeEventListener(event, stopRestoring);
    };
  }, [page, location.key, location.hash, navigation]);
}

export function PageTransition({ children }: { children: ReactNode }) {
  const { pathname } = useLocation();
  const page = pathname.replace(/^\/m\/[^/]+\/[^/]+/, "") || "/";
  usePageNavigation(pathname.replace(/^(\/m\/[^/]+)\/[^/]+/, "$1"));
  return (
    <ContentTransition change={page} kind="page" enter>
      {children}
    </ContentTransition>
  );
}
