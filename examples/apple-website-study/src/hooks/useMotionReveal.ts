import { useEffect } from "react";

export function useMotionReveal() {
  useEffect(() => {
    const revealItems = Array.from(document.querySelectorAll<HTMLElement>(".motion-reveal"));
    const demoRoot = document.querySelector<HTMLElement>(".apple-demo");
    const buyingSection = document.querySelector<HTMLElement>(".buying-section");
    const canExpandBuying = window.matchMedia("(min-width: 735px)").matches;
    const cleanupCallbacks: Array<() => void> = [];

    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      revealItems.forEach((item) => item.classList.add("is-visible"));
      if (canExpandBuying) demoRoot?.classList.add("apple-demo--buying-expanded");
      return undefined;
    }

    const markVisible = (item: HTMLElement) => {
      item.classList.add("is-visible");
    };

    const markVisibleItemsInView = () => {
      revealItems.forEach((item) => {
        if (item.classList.contains("is-visible")) return;
        const rect = item.getBoundingClientRect();
        if (rect.top < window.innerHeight * 0.98 && rect.bottom > 0) markVisible(item);
      });
    };

    revealItems.forEach((item) => {
      const rect = item.getBoundingClientRect();
      if (rect.top < window.innerHeight && rect.bottom > 0) markVisible(item);
    });

    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          markVisible(entry.target as HTMLElement);
          observer.unobserve(entry.target);
        });
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.08 },
    );

    revealItems.forEach((item) => {
      if (!item.classList.contains("is-visible")) observer.observe(item);
    });
    cleanupCallbacks.push(() => observer.disconnect());

    window.addEventListener("scroll", markVisibleItemsInView, { passive: true });
    markVisibleItemsInView();
    cleanupCallbacks.push(() => window.removeEventListener("scroll", markVisibleItemsInView));

    if (canExpandBuying && demoRoot && buyingSection) {
      const expandBuying = () => {
        demoRoot.classList.add("apple-demo--buying-expanded");
      };
      const expandBuyingWhenApproached = () => {
        const rect = buyingSection.getBoundingClientRect();
        if (window.scrollY > 80 || rect.top < window.innerHeight * 0.95) expandBuying();
      };
      const buyingObserver = new IntersectionObserver(
        (entries) => {
          if (!entries.some((entry) => entry.isIntersecting)) return;
          expandBuying();
          buyingObserver.disconnect();
        },
        { rootMargin: "0px 0px -35% 0px", threshold: 0.01 },
      );
      buyingObserver.observe(buyingSection);
      cleanupCallbacks.push(() => buyingObserver.disconnect());

      window.addEventListener("scroll", expandBuyingWhenApproached, { passive: true });
      expandBuyingWhenApproached();
      cleanupCallbacks.push(() => window.removeEventListener("scroll", expandBuyingWhenApproached));
    }

    return () => {
      cleanupCallbacks.forEach((cleanup) => cleanup());
    };
  }, []);
}
