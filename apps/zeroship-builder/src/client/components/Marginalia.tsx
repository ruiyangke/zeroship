// ─── Marginalia — left rail with vol/issue/stamp ─────────────────
//
// Editorial flair for pages that have a left rail. Vol. I, an
// issue line, the studio hours, and a small tomato-outlined `est.`
// stamp. Hidden on narrow viewports.

import { useMemo } from "react";

export function Marginalia() {
  const issueNum = useMemo(() => {
    // Stable per-month so creators see it tick over occasionally.
    const m = new Date().getMonth() + 1;
    return String(m).padStart(2, "0");
  }, []);

  return (
    <aside className="hidden lg:block pt-12 select-none" aria-hidden="true">
      <div className="font-serif italic text-[18px] text-ink mb-1">Vol. I</div>
      <div className="font-sans text-[10.5px] uppercase tracking-[0.18em] text-pencil leading-[1.8]">
        Issue {issueNum} · 2026<br />
        Studio open<br />
        Mon–Fri, 09–18
      </div>
      <div
        className="mt-12 flex h-[92px] w-[92px] flex-col items-center justify-center rounded-full border border-tomato text-tomato"
        style={{ transform: "rotate(-7deg)" }}
      >
        <em className="font-serif not-italic text-[17px] font-medium leading-none mb-1" style={{ fontStyle: "italic" }}>
          est.
        </em>
        <span className="font-sans text-[8.5px] font-bold uppercase tracking-[0.2em] leading-[1.2] text-center">
          Twenty<br />twenty-six
        </span>
      </div>
    </aside>
  );
}
